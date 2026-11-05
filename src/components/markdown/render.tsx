import type { ReactNode } from 'react';
import type {
  Code,
  Heading,
  Html,
  Image,
  InlineCode,
  Link,
  LinkReference,
  List,
  ListItem,
  PhrasingContent,
  RootContent,
  Table,
  Text,
} from 'mdast';
import { sanitizeUri } from 'micromark-util-sanitize-uri';

/**
 * mdast → React 的渲染层。模型输出的 Markdown 属**不可信输入**，这里的三条硬约束：
 * 1. 全程只产出 React 元素，绝不 dangerouslySetInnerHTML——`html` 节点按字面文本显示，
 *    既不注入 DOM，也不把用户看得见的信息吞掉；
 * 2. 链接与图片只放行白名单协议，`javascript:` / `data:` 一律降级成纯文本；
 * 3. 传给 `sanitizeUri` 的值先剥掉控制字符与空白，否则 `java\tscript:` 这类写法能绕过协议判断。
 */

/** 可点击协议白名单：桌面端不放开 file:，相对链接按文档内跳转处理。 */
const SAFE_LINK = /^(?:https?:|mailto:)/i;
const SAFE_IMAGE = /^https?:/i;

/** 剥掉 CommonMark 允许的插入字符（控制符与空白），避免协议判断被拆开绕过。 */
function stripInsertions(value: string): string {
  return value.replace(/[\u0000-\u0020\u007f-\u009f]+/g, '');
}

/** 链接目标：白名单协议给 href，其余返回 undefined 由调用方降级成文本。 */
function safeHref(url: string | null | undefined): string | undefined {
  if (!url) return undefined;
  const raw = stripInsertions(url);
  const colon = raw.indexOf(':');
  const slash = raw.indexOf('/');
  const absolute = colon > 0 && (slash === -1 || colon < slash);
  if (absolute && !SAFE_LINK.test(raw)) return undefined;
  return sanitizeUri(raw) ?? undefined;
}

function imageSrc(url: string | null | undefined): string | undefined {
  if (!url) return undefined;
  const raw = stripInsertions(url);
  return SAFE_IMAGE.test(raw) ? (sanitizeUri(raw) ?? undefined) : undefined;
}

/** 子节点序列 → React 数组，key 取序号（同层节点由源文本顺序唯一确定）。 */
function renderChildren(nodes: readonly PhrasingContent[]): ReactNode[] {
  return nodes.map((node, index) => renderPhrasing(node, index));
}

function renderPhrasing(node: PhrasingContent, key: number): ReactNode {
  switch (node.type) {
    case 'text':
      return (node as Text).value;
    case 'strong':
      return (
        <strong key={key}>{renderChildren(node.children as PhrasingContent[])}</strong>
      );
    case 'emphasis':
      return <em key={key}>{renderChildren(node.children as PhrasingContent[])}</em>;
    case 'delete':
      return <del key={key}>{renderChildren(node.children as PhrasingContent[])}</del>;
    case 'inlineCode':
      return <code key={key}>{(node as InlineCode).value}</code>;
    case 'break':
      return <br key={key} />;
    case 'link': {
      const href = safeHref((node as Link).url);
      const children = renderChildren(node.children as PhrasingContent[]);
      // 协议不在白名单：保留文字、去掉链接，而不是把危险 URL 挂上 href。
      if (href === undefined) return <span key={key}>{children}</span>;
      return (
        <a key={key} href={href} target="_blank" rel="noreferrer noopener">
          {children}
        </a>
      );
    }
    case 'image': {
      const src = imageSrc((node as Image).url);
      const alt = (node as Image).alt ?? '';
      if (src === undefined) return alt;
      return <img key={key} src={src} alt={alt} loading="lazy" />;
    }
    // 引用式链接/图片在收口整段解析前可能还没有定义；先按字面文本走，
    // 收口后由完整解析自愈（与 dsh 声明的同一处已知偏差）。
    case 'linkReference':
      return (node as LinkReference).identifier ?? renderChildren(node.children as PhrasingContent[]);
    case 'html':
      return <span key={key}>{(node as Html).value}</span>;
    default:
      return null;
  }
}

function renderList(node: List, key: number): ReactNode {
  const items = node.children.map((item, index) => renderListItem(item, index));
  const isTask = node.children.some((item) => item.checked !== null);
  return (
    <ul key={key} className={isTask ? 'md-task-list' : undefined}>
      {items}
    </ul>
  );
}

function renderListItem(node: ListItem, key: number): ReactNode {
  const children = node.children.map((child, index) => renderBlock(child, index));
  if (node.checked === null || node.checked === undefined) {
    return <li key={key}>{children}</li>;
  }
  // 任务清单只读展示：勾选状态来自模型输出，不给它一个能改状态的控件。
  return (
    <li key={key} className="md-task-item">
      <input type="checkbox" checked={node.checked} readOnly />
      <span className="md-task-body">{children}</span>
    </li>
  );
}

function renderCode(node: Code, key: number): ReactNode {
  const lang = node.lang ?? '';
  return (
    <div className="md-code-block" key={key} data-lang={lang || undefined}>
      <pre>
        <code className={lang ? `language-${lang}` : undefined}>{node.value}</code>
      </pre>
    </div>
  );
}

function renderTable(node: Table, key: number): ReactNode {
  const align = (index: number) => {
    const value = node.align?.[index];
    return value === 'left' || value === 'center' || value === 'right'
      ? { textAlign: value }
      : undefined;
  };
  const [head, ...body] = node.children;
  return (
    <div className="md-table-scroll" key={key}>
      <table>
        {head && (
          <thead>
            <tr>
              {head.children.map((cell, index) => (
                <th key={index} style={align(index)}>
                  {renderChildren(cell.children as PhrasingContent[])}
                </th>
              ))}
            </tr>
          </thead>
        )}
        <tbody>
          {body.map((row, rowIndex) => (
            <tr key={rowIndex}>
              {row.children.map((cell, index) => (
                <td key={index} style={align(index)}>
                  {renderChildren(cell.children as PhrasingContent[])}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function renderBlock(node: RootContent, key: number): ReactNode {
  switch (node.type) {
    case 'paragraph':
      return <p key={key}>{renderChildren(node.children as PhrasingContent[])}</p>;
    case 'heading': {
      const depth = (node as Heading).depth;
      const children = renderChildren(node.children as PhrasingContent[]);
      const Tag = `h${Math.min(depth, 6)}` as 'h1';
      return <Tag key={key}>{children}</Tag>;
    }
    case 'thematicBreak':
      return <hr key={key} />;
    case 'blockquote':
      return (
        <blockquote key={key}>
          {node.children.map((child, index) => renderBlock(child, index))}
        </blockquote>
      );
    case 'list': {
      const ordered = (node as List).ordered;
      if (!ordered) return renderList(node as List, key);
      const items = node.children.map((item, index) => renderListItem(item, index));
      return (
        <ol key={key} start={(node as List).start ?? undefined}>
          {items}
        </ol>
      );
    }
    case 'code':
      return renderCode(node as Code, key);
    case 'table':
      return renderTable(node as Table, key);
    case 'html':
      return <p key={key}>{(node as Html).value}</p>;
    default:
      return null;
  }
}

/** 顶层块序列 → React 元素（与增量层的块切分共用同一渲染路径）。 */
export function renderBlocks(nodes: readonly RootContent[]): ReactNode[] {
  return nodes.map((node, index) => renderBlock(node, index));
}
