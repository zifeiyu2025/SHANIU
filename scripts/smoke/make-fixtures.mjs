// 冒烟夹具：在隔离工作区里生成一张确定尺寸的 PNG 与一份单页 PDF。
// 尺寸与文本都写进断言，避免「渲染出个空白 canvas 也算过」。
import { deflateSync } from 'node:zlib'
import { writeFileSync } from 'node:fs'
import path from 'node:path'

export const FIXTURE_PNG = { name: 'media-smoke.png', width: 64, height: 64 }
export const FIXTURE_PDF = { name: 'sample.pdf', text: 'SMOKE-PDF-42' }

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n
  for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
  return c >>> 0
})

function crc32(buf) {
  let c = 0xffffffff
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8)
  return (c ^ 0xffffffff) >>> 0
}

function pngChunk(type, data) {
  const length = Buffer.alloc(4)
  length.writeUInt32BE(data.length)
  const body = Buffer.concat([Buffer.from(type, 'latin1'), data])
  const crc = Buffer.alloc(4)
  crc.writeUInt32BE(crc32(body))
  return Buffer.concat([length, body, crc])
}

/** 纯 Rust 无依赖的 64×64 单色 PNG（RGB8，无透明） */
function makePng(width, height) {
  const ihdr = Buffer.alloc(13)
  ihdr.writeUInt32BE(width, 0)
  ihdr.writeUInt32BE(height, 4)
  ihdr[8] = 8 // bit depth
  ihdr[9] = 2 // color type: truecolor
  const row = Buffer.alloc(1 + width * 3)
  for (let x = 0; x < width; x += 1) {
    // 一行内做渐变，肉眼与解码器都能区分「有图」和「空白」
    row[1 + x * 3] = (x * 4) % 256
    row[1 + x * 3 + 1] = 32
    row[1 + x * 3 + 2] = 96
  }
  const raw = Buffer.concat(Array.from({ length: height }, () => row))
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk('IHDR', ihdr),
    pngChunk('IDAT', deflateSync(raw)),
    pngChunk('IEND', Buffer.alloc(0)),
  ])
}

/** 单页 PDF：Helvetica 写一行固定文本，PDF.js 必须有字可渲染 */
function makePdf(text) {
  const stream = `BT /F1 18 Tf 20 100 Td (${text}) Tj ET`
  const bodies = [
    '<</Type/Catalog/Pages 2 0 R>>',
    '<</Type/Pages/Kids[3 0 R]/Count 1>>',
    '<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>',
    `<</Length ${stream.length}>>stream\n${stream}\nendstream`,
    '<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>',
  ]
  let out = '%PDF-1.4\n'
  const offsets = []
  bodies.forEach((body, index) => {
    offsets.push(out.length)
    out += `${index + 1} 0 obj\n${body}\nendobj\n`
  })
  const xref = out.length
  out += `xref\n0 ${bodies.length + 1}\n0000000000 65535 f \n`
  out += offsets.map((offset) => `${String(offset).padStart(10, '0')} 00000 n \n`).join('')
  out += `trailer\n<</Size ${bodies.length + 1}/Root 1 0 R>>\nstartxref\n${xref}\n%%EOF\n`
  return Buffer.from(out, 'latin1')
}

/** @param {string} workspaceDir 已存在的隔离工作区目录 */
export function writeFixtures(workspaceDir) {
  const png = path.join(workspaceDir, FIXTURE_PNG.name)
  const pdf = path.join(workspaceDir, FIXTURE_PDF.name)
  writeFileSync(png, makePng(FIXTURE_PNG.width, FIXTURE_PNG.height))
  writeFileSync(pdf, makePdf(FIXTURE_PDF.text))
  writeFileSync(
    path.join(workspaceDir, 'note.md'),
    `# 冒烟夹具\n\n![相对图](${FIXTURE_PNG.name})\n\nPDF: ${FIXTURE_PDF.name}\n`,
  )
  return { png, pdf }
}
