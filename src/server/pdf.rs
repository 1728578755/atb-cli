//! 极简图片型 PDF 生成器：把一张 JPEG 包成（可能多页的）PDF。
//!
//! 用途只有一个：页面禁止调试器注入时（`Page.printToPDF` 不可用），
//! daemon 用整页拼图（`captureFullPage`）得到的 JPEG 合成 PDF 交付。
//! 文字不可选中，语义上是「整页截图」而不是真正的排版 PDF。
//!
//! 二进制安全是这里的硬要求：JPEG 的字节必须原封不动地写进内容流
//! （`/Filter /DCTDecode`），任何经过 `String` 的路径都会破坏 0x80 以上的字节。
//!
//! 超长页面（CSS 高度换算后超过 PDF 14400pt 上限）会被切成多页：**不重编码 JPEG**，
//! 而是让每一页引用同一个 Image XObject，用内容流的平移矩阵（`cm`）把不同的纵向
//! 条带挪进各自的 MediaBox。这样既满足页面上限，又保持字节级无损。

use anyhow::{anyhow, Result};

/// PDF 规范的页面尺寸上限（Acrobat 的 14400 用户单位）。
pub(crate) const PDF_PAGE_HEIGHT_LIMIT_PT: f64 = 14_400.0;
/// 切片时留的余量：贴着 14400 生成的页面在部分阅读器里会被判越界。
const PDF_PAGE_HEIGHT_MARGIN_PT: f64 = 200.0;

/// 图片型 PDF 的产物：字节 + 实际生效的页面几何。
///
/// 调用方（`capture.rs`）要把这些值如实回显给用户，不能再拿请求里的
/// paper/landscape/scale 冒充「已生效」。
pub(crate) struct ImagePdfOutput {
    pub(crate) bytes: Vec<u8>,
    /// 实际页数（超长页面切片后可能 > 1）。
    pub(crate) pages: usize,
    /// 页面宽度（pt）。
    pub(crate) width_pt: f64,
    /// 整张图的逻辑高度（pt）；多页时 = `page_height_pt * pages`。
    pub(crate) height_pt: f64,
    /// 单页高度（pt），恒 ≤ [`PDF_PAGE_HEIGHT_LIMIT_PT`]。
    pub(crate) page_height_pt: f64,
}

/// 把 JPEG 包装成 PDF。
///
/// 页面尺寸按 96dpi 换算（CSS px → pt）：`pt = px * 72 / 96`。
/// 单页时输出 5 个对象（Catalog / Pages / Page / 内容流 / 图像 XObject）；
/// 多页时每个切片各有一对 Page + 内容流对象，共用同一个图像 XObject。
/// 末尾写交叉引用表与 `startxref`，偏移量全部按真实字节位置计算。
pub(crate) fn image_pdf(jpeg: &[u8], width_px: u32, height_px: u32) -> Result<ImagePdfOutput> {
    if jpeg.is_empty() {
        return Err(anyhow!("image_pdf: JPEG 数据为空，拒绝生成 PDF"));
    }
    if width_px == 0 || height_px == 0 {
        return Err(anyhow!(
            "image_pdf: 页面尺寸非法（{width_px}x{height_px}），拒绝生成 PDF"
        ));
    }
    let width_pt = f64::from(width_px) * 72.0 / 96.0;
    let height_pt = f64::from(height_px) * 72.0 / 96.0;

    // 页高上限（含余量）：单页塞得下就保持原样，塞不下才切。
    let max_page_pt = PDF_PAGE_HEIGHT_LIMIT_PT - PDF_PAGE_HEIGHT_MARGIN_PT;
    let pages = if height_pt <= max_page_pt {
        1
    } else {
        (height_pt / max_page_pt).ceil() as usize
    };
    // 均分而不是「前 N-1 页满、最后一页只剩一条」：均分后每页高度一致，
    // 阅读器里翻页观感稳定，也避免出现 1pt 高的碎页。
    let page_height_pt = height_pt / pages as f64;

    // 对象号：1 Catalog、2 Pages、第 i 页 = 3+2i、其内容流 = 4+2i、图像 = 3+2*pages。
    let image_object = 3 + 2 * pages;

    let mut out: Vec<u8> = Vec::with_capacity(jpeg.len() + 1024 * pages.max(1));
    // 按对象号升序写入，所以 offsets[i] 就是对象 i+1 的字节偏移，xref 直接顺序写。
    let mut offsets: Vec<usize> = Vec::with_capacity(image_object);

    // `%` 注释行里的 4 个高位字节是 PDF 的「二进制文件」标记，阅读器据此按二进制处理。
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");

    offsets.push(out.len());
    out.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    offsets.push(out.len());
    let kids = (0..pages)
        .map(|index| format!("{} 0 R", 3 + 2 * index))
        .collect::<Vec<_>>()
        .join(" ");
    out.extend_from_slice(
        format!("2 0 obj\n<< /Type /Pages /Kids [{kids}] /Count {pages} >>\nendobj\n").as_bytes(),
    );

    for index in 0..pages {
        let page_object = 3 + 2 * index;
        let content_object = 4 + 2 * index;
        // 第 index 页（自上而下）对应的图像 y 偏移：图像整幅绘制后向下平移。
        let offset_y = height_pt - (index + 1) as f64 * page_height_pt;
        // `-0.0` 会被格式化成 "-0.00"，单页输出要与此前完全一致。
        let translate = if offset_y == 0.0 { 0.0 } else { -offset_y };
        let content = format!(
            "q {width_pt:.2} 0 0 {height_pt:.2} 0 {translate:.2} cm /Im{image_object} Do Q\n"
        );

        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{page_object} 0 obj\n<< /Type /Page /Parent 2 0 R \
                 /MediaBox [0 0 {width_pt:.2} {page_height_pt:.2}] \
                 /Resources << /XObject << /Im{image_object} {image_object} 0 R >> >> \
                 /Contents {content_object} 0 R >>\nendobj\n"
            )
            .as_bytes(),
        );

        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{content_object} 0 obj\n<< /Length {} >>\nstream\n",
                content.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(content.as_bytes());
        out.extend_from_slice(b"endstream\nendobj\n");
    }

    offsets.push(out.len());
    out.extend_from_slice(
        format!(
            "{image_object} 0 obj\n<< /Type /XObject /Subtype /Image /Width {width_px} /Height {height_px} \
             /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
            jpeg.len()
        )
        .as_bytes(),
    );
    // 关键：JPEG 原样落盘，长度就是字节数（不是字符数）。
    out.extend_from_slice(jpeg);
    out.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_offset = out.len();
    let size = offsets.len() + 1;
    out.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
            .as_bytes(),
    );
    Ok(ImagePdfOutput {
        bytes: out,
        pages,
        width_pt,
        height_pt,
        page_height_pt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一段结构上像 JPEG 的假数据：SOI + APP0 头 + 填充 + EOI。
    fn fake_jpeg() -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        // 0x80 以上的字节必须原样保留，用来抓「JPEG 被当字符串处理」的回归。
        bytes.extend((0u16..512).map(|index| (index % 256) as u8));
        bytes.extend_from_slice(&[0xff, 0xd9]);
        bytes
    }

    #[test]
    fn image_pdf_has_valid_header_and_lengths() {
        let jpeg = fake_jpeg();
        let output = image_pdf(&jpeg, 1280, 2000).expect("生成 PDF");
        let pdf = output.bytes;
        assert_eq!(output.pages, 1, "普通高度不应切片");
        assert!(pdf.starts_with(b"%PDF-1.4"), "PDF 头不正确");
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/DCTDecode"), "缺少 DCTDecode 过滤器");
        assert!(text.contains("/Width 1280"), "图像宽度未写入");
        assert!(text.contains("/Height 2000"), "图像高度未写入");
        // 96dpi 换算：1280px → 960pt，2000px → 1500pt。
        assert!(
            text.contains("/MediaBox [0 0 960.00 1500.00]"),
            "页面尺寸未换算"
        );
        assert!(
            text.contains(&format!("/Length {}", jpeg.len())),
            "图像流长度必须是真实字节数"
        );
        assert!(pdf.ends_with(b"%%EOF\n"), "缺少 %%EOF 结尾");
        // 交叉引用表偏移必须真的指向 `xref`，否则任何阅读器都会报损坏。
        let startxref = text.rfind("startxref").expect("缺少 startxref");
        let offset_text = text[startxref + "startxref".len()..]
            .split_whitespace()
            .next()
            .expect("缺少 startxref 偏移");
        let offset: usize = offset_text.parse().expect("startxref 偏移不是数字");
        assert_eq!(
            &pdf[offset..offset + 4],
            b"xref",
            "startxref 偏移 {offset} 没有指向交叉引用表"
        );
        // JPEG 字节必须逐字节保留在流里（二进制安全）。
        assert!(
            pdf.windows(jpeg.len())
                .any(|window| window == jpeg.as_slice()),
            "JPEG 数据在写入过程中被改动"
        );
        // xref 每一行的前 10 位是该对象的真实字节偏移，偏一位阅读器就找不到对象。
        for index in 1..=5 {
            let marker = format!("{index} 0 obj");
            let object_offset = pdf
                .windows(marker.len())
                .position(|window| window == marker.as_bytes())
                .unwrap_or_else(|| panic!("互引对象 {index} 缺失"));
            assert!(
                text.contains(&format!("{object_offset:010} 00000 n")),
                "对象 {index} 的 xref 偏移与实际位置不一致"
            );
        }
    }

    #[test]
    fn image_pdf_rejects_empty() {
        assert!(image_pdf(&[], 1280, 2000).is_err());
        assert!(image_pdf(&[0xff, 0xd8], 0, 2000).is_err());
    }

    /// 超长页面必须切片：每页高度 ≤ 14400pt，`/Count` 与实际页对象数一致。
    #[test]
    fn image_pdf_slices_over_tall_pages() {
        let jpeg = fake_jpeg();
        // 40000 CSS px → 30000pt，单页放不下，至少要 3 页。
        let output = image_pdf(&jpeg, 1280, 40_000).expect("生成 PDF");
        assert!(output.pages >= 3, "超长页面没有切片：{}", output.pages);
        assert!(
            output.page_height_pt <= PDF_PAGE_HEIGHT_LIMIT_PT,
            "单页高度 {} 超过 PDF 上限",
            output.page_height_pt
        );
        let text = String::from_utf8_lossy(&output.bytes);
        assert!(
            text.contains(&format!("/Count {}", output.pages)),
            "/Count 与实际页数不一致"
        );
        assert_eq!(
            text.matches("/Type /Page ").count(),
            output.pages,
            "页对象数量与计数不一致"
        );
        // 每页都引用同一个图像对象，JPEG 只存一份。
        assert_eq!(text.matches("/DCTDecode").count(), 1, "JPEG 被重复写入");
        // xref 必须覆盖到最后一个对象（图像对象号 = 3 + 2*pages）。
        let last = 3 + 2 * output.pages;
        assert!(
            text.contains(&format!("{last} 0 obj")),
            "图像对象 {last} 缺失"
        );
    }
}
