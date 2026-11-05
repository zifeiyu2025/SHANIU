import { describe, expect, it } from 'vitest';
import { render } from '@testing-library/react';
import { StreamingText } from '../components/markdown/StreamingText';

/** 收口态渲染：模型输出的 Markdown 应该变成语义 DOM，而不是原样铺字符。 */
function settled(text: string) {
  return render(<StreamingText text={text} streaming={false} />).container;
}

describe('StreamingText Markdown 结构', () => {
  it('标题按层级成元素，正文成段落', () => {
    const el = settled('# 一级\n\n## 二级\n\n正文段落');
    expect(el.querySelector('h1')?.textContent).toBe('一级');
    expect(el.querySelector('h2')?.textContent).toBe('二级');
    expect(el.querySelector('p')?.textContent).toBe('正文段落');
  });

  it('围栏代码块保留语言与原文，不被当成正文', () => {
    const el = settled('```ts\nconst a = 1;\n```');
    const code = el.querySelector('pre code');
    expect(code?.textContent).toBe('const a = 1;');
    expect(code?.className).toContain('language-ts');
    expect(el.querySelector('.md-code-block')).toBeTruthy();
  });

  it('列表、任务清单与有序列表各归其位', () => {
    const ul = settled('- 甲\n- 乙');
    expect(ul.querySelectorAll('li')).toHaveLength(2);
    const ol = settled('1. 第一\n2. 第二');
    expect(ol.querySelector('ol')).toBeTruthy();
    const task = settled('- [x] 已完成\n- [ ] 未完成');
    const boxes = task.querySelectorAll('input[type="checkbox"]');
    expect(boxes).toHaveLength(2);
    expect((boxes[0] as HTMLInputElement).checked).toBe(true);
    // 只读展示：勾选框不该是一个能改状态的活控件
    expect((boxes[0] as HTMLInputElement).readOnly).toBe(true);
  });

  it('行内强调、行内码与 GFM 删除线', () => {
    const el = settled('**粗** 和 *斜* 与 `code` 与 ~~删~~');
    expect(el.querySelector('strong')?.textContent).toBe('粗');
    expect(el.querySelector('em')?.textContent).toBe('斜');
    expect(el.querySelector('code')?.textContent).toBe('code');
    expect(el.querySelector('del')?.textContent).toBe('删');
    // 单个 ~ 不是删除线（区间写法 10~20 必须保持字面）
    expect(settled('区间 10~20').querySelector('del')).toBeNull();
  });

  it('表格渲染成 thead/tbody', () => {
    const el = settled('| 列 | 值 |\n| --- | --- |\n| a | 1 |');
    expect(el.querySelectorAll('th')).toHaveLength(2);
    expect(el.querySelector('tbody td')?.textContent).toBe('a');
  });

  it('引用与分隔线', () => {
    const el = settled('> 引用内容\n\n---\n\n之后');
    expect(el.querySelector('blockquote')?.textContent).toContain('引用内容');
    expect(el.querySelector('hr')).toBeTruthy();
  });
});

describe('StreamingText 不可信输入', () => {
  it('raw HTML 只作为字面文本出现，不进入 DOM', () => {
    const el = settled('<script>window.bad=1</script>\n\n正文');
    expect(el.querySelector('script')).toBeNull();
    expect(el.textContent).toContain('<script>window.bad=1</script>');
    expect((window as unknown as { bad?: number }).bad).toBeUndefined();
  });

  it('图片标签形式的 HTML 同样不产生元素', () => {
    const el = settled('<img src=x onerror="window.pwned=1">');
    expect(el.querySelector('img')).toBeNull();
    expect((window as unknown as { pwned?: number }).pwned).toBeUndefined();
  });

  it('非白名单协议的链接降级为文本，不挂 href', () => {
    const el = settled('[点我](javascript:alert(1))');
    const a = el.querySelector('a');
    expect(a).toBeNull();
    expect(el.textContent).toContain('点我');
  });

  it('拆写协议绕过（java\\tscript:）也拿不到 href', () => {
    const el = settled('[x](java\tscript:alert(1))');
    expect(el.querySelector('a')).toBeNull();
  });

  it('http/https/mailto 与相对链接正常可点', () => {
    const el = settled('[文档](https://example.com/a) 与 [邮件](mailto:a@b.c) 与 [本地](./notes.md)');
    const links = [...el.querySelectorAll('a')];
    expect(links).toHaveLength(3);
    expect(links[0].getAttribute('href')).toBe('https://example.com/a');
    expect(links[0].getAttribute('rel')).toContain('noopener');
    expect(links[1].getAttribute('href')).toBe('mailto:a@b.c');
    expect(links[2].getAttribute('href')).toBe('./notes.md');
  });

  it('非 http(s) 的图片不产生 img 元素，只留 alt', () => {
    const el = settled('![本地图](file:///C:/secret.png)');
    expect(el.querySelector('img')).toBeNull();
    expect(el.textContent).toContain('本地图');
    const ok = settled('![网图](https://example.com/a.png)');
    expect(ok.querySelector('img')?.getAttribute('src')).toBe('https://example.com/a.png');
  });
});

describe('StreamingText 流式中间态', () => {
  it('未闭合围栏在流式期间仍按代码块渲染，不泄漏成文本', () => {
    const view = render(<StreamingText text={'说明\n\n```python\nprint(1)\nprint(2)'} streaming />);
    expect(view.container.querySelector('pre code')).toBeTruthy();
    expect(view.getByTestId('streaming-cursor')).toBeTruthy();
    view.rerender(<StreamingText text={'说明\n\n```python\nprint(1)\nprint(2)\n```'} streaming={false} />);
    expect(view.queryByTestId('streaming-cursor')).toBeNull();
    expect(view.container.querySelector('pre code')?.textContent).toBe('print(1)\nprint(2)');
  });

  it('流式中的行内标记不产生半个元素', () => {
    const view = render(<StreamingText text={'结果是 **重要'} streaming />);
    // 未闭合的 ** 按字面显示，等收口再成 strong
    expect(view.container.textContent).toContain('**重要');
    view.rerender(<StreamingText text={'结果是 **重要**'} streaming={false} />);
    expect(view.container.querySelector('strong')?.textContent).toBe('重要');
  });
});
