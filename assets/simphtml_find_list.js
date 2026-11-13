// ---- 入口挂载----
// 本文件以前是 4 个顶层函数声明（findMainList / findTopGroups / findMatchingElements /
// scoreContainer）：classic script 里那是**可枚举 + 可删除**的全局，站点一行
// `Object.keys(window)` 就能看见，正是 page_net.js 头部批判过的指纹形态；而 stealth
// 的 OWN_GLOBALS 只列了 8 个 `__abc*`，这几个完全漏网（audit 报不到、scrub 不删）。
//
// 现在整体包进 IIFE：其余三个函数只在本文件内互相调用，不需要暴露；
// 只有调用方要按裸标识符用的 findMainList 临时挂到 window（不可枚举 + 可配置 +
// 用完即撤，见 publishTempGlobal），最终页面状态与注入前一致。
;(function () {
function findMainList(startElement = null, opts) {
        const root = startElement || document.body;
        // 调试日志默认静默。以前每次评分 >100 都往**被访页面**的控制台
        // 打一个 container + 分数对象，第三方页面能看见、也会拖慢控制台。
        const debug = !!(opts && opts.debug);
        const MIN_CHILDREN = 8;
        const MAX_CONTAINERS = 20;

        // 全局扫描：收集候选容器，按 l1 + l2*0.1 排序（l2=孙子元素数，捕获表格等多层结构）
        const candidates = [];
        const allEls = root.querySelectorAll('*');
        for (const node of allEls) {
            if (node.closest('svg')) continue;
            const l1 = node.children.length;
            if (l1 < 5) continue;
            let l2 = 0;
            for (const child of node.children) l2 += child.children.length;
            const score = l1 + l2 * 0.1;
            if (score >= MIN_CHILDREN) candidates.push({node, score});
        }
        candidates.sort((a, b) => b.score - a.score);
        const toProcess = candidates.slice(0, MAX_CONTAINERS).map(c => c.node);

        // 对每个容器找候选组并评分
        let allCandidates = [];
        for (const container of toProcess) {
            const topGroups = findTopGroups(container, 3);
            for (const groupInfo of topGroups) {
                const items = findMatchingElements(container, groupInfo.selector);
                if (items.length >= 5) {
                    const score = scoreContainer(container, items) + groupInfo.score;
                    if (score >= 30) {
                        allCandidates.push({ container, selector: groupInfo.selector, items, score });
                    }
                }
            }
        }

        // 按分数降序排列
        allCandidates.sort((a, b) => b.score - a.score);

        // 去重：移除与更高分候选重叠超50%的结果
        const kept = [];
        for (const cand of allCandidates) {
            let dominated = false;
            for (const k of kept) {
                if (k.container.contains(cand.container) || cand.container.contains(k.container)) {
                    const kSet = new Set(k.items);
                    const overlap = cand.items.filter(it => kSet.has(it)).length;
                    if (overlap > cand.items.length * 0.5) { dominated = true; break; }
                }
            }
            if (!dominated) kept.push(cand);
        }

        // 不要给页面元素写 id。历史写法是 `container.id = '_ljq' + n`：那是直接在
        // **第三方页面的 DOM 上留痕** —— 会破坏依赖 id 唯一性的页面逻辑 / 埋点 /
        // `[id^=...]` 形式的 CSS，还会被 MutationObserver 型框架当成一次变更；
        // 多调几次就永久留下 _ljq1..n。改成生成一条不落地 DOM 的路径。
        function containerPath(el) {
            if (!el || el.nodeType !== 1) return '';
            if (el.id) {
                const byId = '#' + CSS.escape(el.id);
                try { if (document.querySelectorAll(byId).length === 1) return byId; } catch (_) {}
            }
            const parts = [];
            let node = el;
            let guard = 0;
            while (node && node.nodeType === 1 && guard < 32) {
                guard += 1;
                let part = node.tagName.toLowerCase();
                const parent = node.parentElement;
                if (parent) {
                    const sameTag = Array.from(parent.children).filter(c => c.tagName === node.tagName);
                    if (sameTag.length > 1) part += ':nth-of-type(' + (sameTag.indexOf(node) + 1) + ')';
                }
                parts.unshift(part);
                node = parent;
                if (node && node.id) { parts.unshift('#' + CSS.escape(node.id)); break; }
            }
            return parts.join(' > ');
        }

        function describeResult(container, items, selector, score) {
            const cTag = container ? container.tagName : null;
            const cId = container ? (container.id || '') : '';
            const cClass = container ? (String(container.className || '').trim()) : '';
            const prefix = container ? containerPath(container) : '';
            const result = {
                containerTag: cTag, containerId: cId, containerClass: cClass,
                // 容器本身的可复用选择器（不依赖元素 id 是否存在）
                containerSelector: prefix || null,
                itemCount: items.length,
            };
            if (selector) result.selector = prefix ? (prefix + ' > ' + selector) : selector;
            if (score !== undefined) result.score = score;
            if (items.length > 0) {
                result.firstItemPreview = items[0].outerHTML.substring(0, 200);
                result.itemTags = items.slice(0, 10).map(el => el.tagName + (el.className ? '.' + String(el.className).trim().split(/\s+/)[0] : ''));
            }
            return result;
        }

        if (kept.length === 0) return [];

        return kept.map(c => describeResult(c.container, c.items, c.selector, c.score));
    }

    function findTopGroups(container, limit) {
        const children = Array.from(container.children).filter(c => !c.closest('svg'));
        const totalChildren = children.length;
        if (totalChildren < 3) return [];

        const minGroupSize = Math.max(3, Math.floor(totalChildren * 0.2));
        const groups = [];

        // 统计标签和类名
        const tagFreq = {}, classFreq = {}, tagMap = {}, classMap = {};

        children.forEach(child => {
            // 统计标签
            const tag = child.tagName.toLowerCase();
            if (tag === "td") return;
            tagFreq[tag] = (tagFreq[tag] || 0) + 1;
            if (!tagMap[tag]) tagMap[tag] = [];
            tagMap[tag].push(child);

            // 统计类名
            if (child.className) {
                child.className.trim().split(/\s+/).forEach(cls => {
                    if (cls) {
                        classFreq[cls] = (classFreq[cls] || 0) + 1;
                        if (!classMap[cls]) classMap[cls] = [];
                        classMap[cls].push(child);
                    }
                });
            }
        });

        // 评分函数
        const scoreGroup = (selector, elements) => {
            const coverage = elements.length / totalChildren;
            let specificity = selector.startsWith('.')
            ? (0.6 + (selector.match(/\./g).length - 1) * 0.1) // 类选择器
            : (selector.includes('.')
               ? (0.7 + (selector.match(/\./g).length) * 0.1) // 标签+类
               : 0.3); // 纯标签
            return (coverage * 0.5) + (specificity * 0.5);
        };

        // 添加标签组
        Object.keys(tagFreq).forEach(tag => {
            if (tag !== "div" && tagFreq[tag] >= minGroupSize) {
                groups.push({
                    selector: tag,
                    elements: tagMap[tag],
                    score: scoreGroup(tag, tagMap[tag]) - 0.5
                });
            }
        });

        // 添加类组
        Object.keys(classFreq).forEach(cls => {
            if (classFreq[cls] >= minGroupSize) {
                const selector = '.' + CSS.escape(cls);
                groups.push({
                    selector,
                    elements: classMap[cls],
                    score: scoreGroup(selector, classMap[cls])
                });
            }
        });
        // 添加标签+类组合
        const topTags = Object.keys(tagFreq).filter(t => tagFreq[t] >= minGroupSize).slice(0, 3);
        const topClasses = Object.keys(classFreq).filter(c => classFreq[c] >= minGroupSize).sort((a, b) => classFreq[b] - classFreq[a]).slice(0, 3);

        // 标签+类
        topTags.forEach(tag => {
            topClasses.forEach(cls => {
                const elements = children.filter(el =>
                                                 el.tagName.toLowerCase() === tag &&
                                                 el.className && el.className.split(/\s+/).includes(cls)
                                                );

                if (elements.length >= minGroupSize) {
                    const selector = tag + '.' + CSS.escape(cls);
                    groups.push({selector, elements, score: scoreGroup(selector, elements)});
                }
            });
        });

        // 多类组合
        for (let i = 0; i < topClasses.length; i++) {
            for (let j = i + 1; j < topClasses.length; j++) {
                const elements = children.filter(el =>
                                                 el.className && el.className.split(/\s+/).includes(topClasses[i]) && el.className.split(/\s+/).includes(topClasses[j]));

                if (elements.length >= minGroupSize) {
                    const selector = '.' + CSS.escape(topClasses[i]) + '.' + CSS.escape(topClasses[j]);
                    groups.push({selector, elements,score: scoreGroup(selector, elements)});
                }
            }
        }
        // 返回得分最高的N个组
        return groups.sort((a, b) => b.score - a.score).slice(0, limit);
    }

    function findMatchingElements(container, selector) {
        try {
            return Array.from(container.querySelectorAll(selector));
        } catch (e) {
            // 处理无效选择器
            console.error('Invalid selector:', selector, e);
            return [];
        }
    }

    function scoreContainer(container, items) {
        if (!container || items.length < 3) return 0;
        // 1. 计算基础面积数据
        const containerRect = container.getBoundingClientRect();
        const containerArea = containerRect.width * containerRect.height;
        if (containerArea < 10000) return 0; // 容器太小

        // 收集列表项面积数据
        const itemAreas = [];
        // 每个 item 每轮只读一次布局，坐标分组时复用同一份 rect。
        // 以前在 items.map 里又调两次 getBoundingClientRect，等于 3 倍布局读。
        const rects = [];
        let totalItemArea = 0;
        let visibleItems = 0;

        items.forEach(item => {
            const rect = item.getBoundingClientRect();
            rects.push(rect);
            const area = rect.width * rect.height;
            if (area > 0) {
                totalItemArea += area;
                itemAreas.push(area);
                visibleItems++;
            }
        });
        // 如果可见项太少，返回低分
        if (visibleItems < 3) return 0;
        // 防止异常值：确保面积不超过容器
        totalItemArea = Math.min(totalItemArea, containerArea * 0.98);
        const areaRatio = totalItemArea / containerArea;
        // 3. 计算各项评分 - 使用线性插值而非阶梯
        // 3.2 面积比评分 - 最多40分，连续曲线
        // 使用sigmoid函数让评分更平滑
        const areaScore = 40 / (1 + Math.exp(-12 * (areaRatio - 0.4)));

        // 3.3 均匀性评分 - 最多20分，连续曲线
        let uniformityScore = 0;
        if (itemAreas.length >= 3) {
            const mean = itemAreas.reduce((sum, area) => sum + area, 0) / itemAreas.length;
            const variance = itemAreas.reduce((sum, area) => sum + Math.pow(area - mean, 2), 0) / itemAreas.length;
            const cv = mean > 0 ? Math.sqrt(variance) / mean : 1;
            // 指数衰减函数，cv越小分数越高
            uniformityScore = 20 * Math.exp(-2.5 * cv);
        }

        const baseScore = Math.log2(visibleItems) * 5 + Math.floor(visibleItems / 5) * 0.25;
        const rawCountScore = Math.min(40, baseScore);
        const countScore = rawCountScore * Math.max(0.1, uniformityScore / 20);

        // 3.4 容器尺寸评分 - 最多15分，连续曲线
        const viewportArea = window.innerWidth * window.innerHeight;
        const containerViewportRatio = containerArea / viewportArea;
        const sizeScore = 2 * (1 - 1/(1 + Math.exp(-10 * (containerViewportRatio - 0.25))));

        let layoutScore = 0;
        if (items.length >= 3) {
            // 坐标分组并计算行列数（复用上面的 rects，不再重复读布局）
            const uniqueRows = new Set(rects.map(r => Math.round(r.top / 5) * 5)).size;
            const uniqueCols = new Set(rects.map(r => Math.round(r.left / 5) * 5)).size;
            // 如果是单行或单列，直接给满分；否则评估网格质量
            if (uniqueRows === 1 || uniqueCols === 1) { layoutScore = 20;
            } else {
                const coverage = Math.min(1, items.length / (uniqueRows * uniqueCols));
                const efficiency = Math.max(0, 1 - (uniqueRows + uniqueCols) / (2 * items.length));
                layoutScore = 20 * (0.7 * coverage + 0.3 * efficiency);
            }
        }

        // 总分 - 仍然保持100分左右的总分
        const totalScore = countScore + areaScore + uniformityScore + layoutScore + sizeScore;

        // 调试信息只在 opts.debug 时输出，默认不污染页面控制台。
        if (debug && totalScore > 100)
            console.log(container, {
                total: totalScore.toFixed(2),
                count: countScore.toFixed(2),
                areaRatio: areaRatio.toFixed(2),
                area: areaScore.toFixed(2),
                uniformity: uniformityScore.toFixed(2),
                size: sizeScore.toFixed(2),
                layout: layoutScore.toFixed(2)
            });

        return totalScore;
    }

// 临时全局入口：与 simphtml_opt.js 的 publishTempGlobal 同一套语义。
///
/// 调用方（daemon 的 `scan --list`，见 `html::js_find_main_list`）把本文件与
/// `return findMainList(...)` 拼在同一段脚本里，所以入口必须能被裸标识符解析到。
/// 挂 window 是最小侵入的方式；非枚举（Object.keys 看不到）、可配置（scrub 能删），
/// 并在本次任务末尾还原/摘掉 —— 页面最终看不到任何残留。
function publishTempGlobal(name, value) {
    let previous = null;
    let had = false;
    try {
        had = Object.prototype.hasOwnProperty.call(window, name);
        previous = had ? Object.getOwnPropertyDescriptor(window, name) : null;
    } catch (_) {}
    try {
        Object.defineProperty(window, name, {
            value: value, writable: true, enumerable: false, configurable: true
        });
    } catch (_) {
        try { window[name] = value; } catch (_) {}
    }
    try {
        setTimeout(function () {
            if (had && previous) {
                try { Object.defineProperty(window, name, previous); return; } catch (_) {}
            }
            try { if (window[name] === value) delete window[name]; } catch (_) {}
        }, 0);
    } catch (_) {}
}

publishTempGlobal('findMainList', findMainList);
})();