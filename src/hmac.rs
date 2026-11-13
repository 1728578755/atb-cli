//! 最小密码学实现：SHA-256 / HMAC-SHA256 / 小写 hex。
//!
//! 为什么手写而不是引依赖：本项目要求离线可构建、不新增第三方 crate。这里的
//! 用途只有一个——daemon 在 WS 握手后向扩展证明「我持有 ws-trust.json 里的 token」，
//! 输入是固定的短消息（`nonce|extension_id`），不需要流式、不需要高性能。
//!
//! 正确性靠标准测试向量兜底（见文件末尾）：SHA-256 用 `"abc"`，HMAC 用
//! RFC 4231 Test Case 2。两处都覆盖到分组补齐、密钥填充与 ipad/opad 叠加。

/// SHA-256 轮常量（FIPS 180-4 §4.2.2）。
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 初始哈希值（FIPS 180-4 §5.3.3）。
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// HMAC / SHA-256 的输入分组长度（字节）。
const BLOCK: usize = 64;

/// 单块压缩函数（FIPS 180-4 §6.2.2）。
fn compress(state: &mut [u32; 8], chunk: &[u8]) {
    debug_assert_eq!(chunk.len(), BLOCK);
    let mut w = [0u32; 64];
    for (index, word) in w.iter_mut().take(16).enumerate() {
        let at = index * 4;
        *word = u32::from_be_bytes([chunk[at], chunk[at + 1], chunk[at + 2], chunk[at + 3]]);
    }
    for index in 16..64 {
        let s0 =
            w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
        let s1 =
            w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
        w[index] = w[index - 16]
            .wrapping_add(s0)
            .wrapping_add(w[index - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[index])
            .wrapping_add(w[index]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(value);
    }
}

/// SHA-256 摘要。
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state = H0;
    // 消息长度按 **比特** 记，附加在最后 8 字节（大端）。64 位长度对内存里
    // 的实际输入永远够用，`wrapping_mul` 只是避免理论上的溢出 panic。
    let bit_len = (data.len() as u64).wrapping_mul(8);

    let mut padded = Vec::with_capacity(data.len() + BLOCK * 2);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % BLOCK != BLOCK - 8 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for at in (0..padded.len()).step_by(BLOCK) {
        compress(&mut state, &padded[at..at + BLOCK]);
    }

    let mut out = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// HMAC-SHA256（RFC 2104）。
///
/// 密钥超过分组长度时先摘要成 32 字节（RFC 2104 §2 的 `H(K)` 分支），
/// 再补零到 64 字节；两侧分别异或 ipad/opad。
pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut inner = Vec::with_capacity(BLOCK + message.len());
    inner.extend(block.iter().map(|byte| byte ^ 0x36));
    inner.extend_from_slice(message);
    let inner_digest = sha256(&inner);

    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend(block.iter().map(|byte| byte ^ 0x5c));
    outer.extend_from_slice(&inner_digest);
    sha256(&outer)
}

/// 小写 hex：与扩展侧 WebCrypto 的 `hex()` 输出口径一致，逐字比较才有意义。
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 标准向量（FIPS 180-4 附录 B.1）：补齐逻辑与末尾长度字段的最小覆盖。
    #[test]
    fn sha256_matches_the_standard_vector_for_abc() {
        assert_eq!(
            hex_lower(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 空输入同样是常见边界：只有补齐块，没有消息字节。
        assert_eq!(
            hex_lower(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // 跨过分组边界的输入（56/64 字节附近最容易把长度字段写错）。
        assert_eq!(
            hex_lower(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// RFC 4231 Test Case 2：验证 ipad/opad、密钥补齐与消息拼接。
    #[test]
    fn hmac_sha256_matches_rfc4231_case2() {
        assert_eq!(
            hex_lower(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// 超长密钥（>64 字节）走 `H(K)` 分支，单独用 RFC 4231 Test Case 6 覆盖。
    #[test]
    fn hmac_sha256_hashes_keys_longer_than_the_block() {
        let key = [0xaa_u8; 131];
        assert_eq!(
            hex_lower(&hmac_sha256(
                &key,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn hex_lower_pads_single_digits() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }
}
