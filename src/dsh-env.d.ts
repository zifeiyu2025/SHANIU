// DSH 移植代码的环境声明（对应 DSH scripts/types/client-assets + client-build-environment）
// 只覆盖模块声明与构建期常量，不声明任何运行时行为。

declare module '*.module.css' {
  const classes: { readonly [key: string]: string }
  export default classes
}

declare module '*.css'

declare module '*.svg' { const url: string; export default url }
declare module '*.png' { const url: string; export default url }
declare module '*.webp' { const url: string; export default url }
declare module '*.jpg' { const url: string; export default url }
declare module '*.jpeg' { const url: string; export default url }

// 构建期常量（DSH client-build-environment 原样）：bundler 替换，浏览器端无 process
declare const process: {
  readonly env: {
    readonly NODE_ENV?: string
    readonly [name: `DSH_CLIENT_${string}`]: string | undefined
  }
}
