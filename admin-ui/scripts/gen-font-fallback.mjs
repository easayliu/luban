// 一次性脚本：算出「度量对齐后备字体」的 @font-face，结果固化进 src/index.css。
// 用法：cd admin-ui && node gen-fallback.mjs
import { readMetrics, getMetricsForFamily, generateFontFace } from 'fontaine'

const SETS = [
  {
    family: 'Fira Sans',
    weights: [400, 500, 600, 700],
    file: (w) => `./node_modules/@fontsource/fira-sans/files/fira-sans-latin-${w}-normal.woff2`,
    // 按平台命中率排序：浏览器取第一个 local() 能解析到的那条。
    // Helvetica Neue = macOS，Segoe UI = Windows，Roboto = Android/部分 Linux，Arial 兜底。
    fallbacks: ['Helvetica Neue', 'Segoe UI', 'Roboto', 'Arial'],
  },
  {
    family: 'Fira Code',
    weights: [400, 500, 700],
    file: (w) => `./node_modules/@fontsource/fira-code/files/fira-code-latin-${w}-normal.woff2`,
    // 度量库里有的等宽字体只有 Courier New / Roboto Mono / Ubuntu Mono，
    // 其中只有 Courier New 各平台都预装。
    fallbacks: ['Courier New'],
  },
]

let out = ''
for (const set of SETS) {
  for (const w of set.weights) {
    const metrics = await readMetrics(new URL(set.file(w), import.meta.url))
    if (!metrics) throw new Error(`读不到字体度量：${set.family} ${w}`)
    for (const fb of set.fallbacks) {
      const fbMetrics = await getMetricsForFamily(fb)
      if (!fbMetrics) throw new Error(`度量库里没有后备字体：${fb}`)
      const face = generateFontFace(metrics, {
        name: `${set.family} fallback`,
        font: fb,
        metrics: fbMetrics,
      }).trim()
      // 每个字重单独一条：size-adjust 取决于该字重的平均字宽，逐重不同。
      out += `${face.slice(0, -1).trimEnd()}\n  font-weight: ${w};\n}\n`
    }
  }
}
process.stdout.write(out)
