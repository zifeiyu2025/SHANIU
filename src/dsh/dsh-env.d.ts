// DSH 移植代码的环境声明（对应 DSH scripts/types/client-assets）
// process 由 @types/node 提供，不在此重复声明（重复会遮蔽完整 NodeJS.Process）。

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
