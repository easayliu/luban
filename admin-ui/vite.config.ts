import { defineConfig, type PluginOption } from 'vite'
import react from '@vitejs/plugin-react-swc'
import tailwindcss from '@tailwindcss/vite'
import path from 'path'

/**
 * 剥掉 Fontsource 的 `.woff` 回退，只留 woff2。
 *
 * woff2 从 2016 年起就是全平台可用（唯一的例外 IE11 早已不在支持范围），而这些字体文件
 * 会跟着 dist 一起被 rust-embed 编译进 luban 二进制——多带一份等价格式等于凭空给每个
 * 发行版塞进 150 KB 永远不会被请求的字节。
 */
function dropWoff1(): PluginOption {
  return {
    name: 'luban-drop-woff1',
    transform(code, id) {
      if (!id.includes('.css')) return null
      // 此处 url() 已被 Vite 换成 __VITE_ASSET__ 占位符，认不了后缀，靠 format("woff")
      // 精确定位（woff2 的引号内是 woff2，不会被这条误伤）。
      return { code: code.replace(/,\s*url\([^)]*\)\s*format\(["']woff["']\)/g, ''), map: null }
    },
    // 去掉 CSS 引用还不够：资源在更早的阶段就已登记，不在这里删会照样落进 dist。
    generateBundle(_options, bundle) {
      for (const name of Object.keys(bundle)) {
        if (name.endsWith('.woff')) delete bundle[name]
      }
    },
  }
}

// luban 网页在根路径 `/` 提供服务；开发时 /api 代理到本地 luban 后端。
export default defineConfig({
  plugins: [
    react(),
    tailwindcss(),
    dropWoff1(),
    // 注：度量对齐的后备字体没有走 fontaine 的构建期插件，而是用它的计算函数一次性算好、
    // 固化在 src/index.css 里（生成脚本 scripts/gen-font-fallback.mjs）。插件方式在本项目
    // 行不通：它只改写 `font-family:` 声明，看不见 Tailwind v4 `@theme` 里的自定义属性，
    // 生成的后备族名压根不会被引用；且其默认命名取字族名首词，"Fira Sans" 与 "Fira Code"
    // 会双双塌缩成 "Fira fallback"，两套相差 30% 的度量顶着同一个名字互相覆盖。
  ],
  base: '/',
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  server: {
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:4600',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
})
