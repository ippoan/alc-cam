// CRX3 のヘッダに入っている公開鍵から拡張 ID を出す (タグの CI で「ID が変わっていない」ことを確かめる)。
//   node scripts/crx-id.mjs alc-cam.crx            → ID を出す
//   node scripts/crx-id.mjs alc-cam.crx <期待の ID> → 違えば exit 1
// ID = SHA256(公開鍵 DER) の先頭 16 バイトを 16 進にし、0-f を a-p に置き換えたもの。
// ヘッダは CrxFileHeader (protobuf): 2 = sha256_with_rsa (AsymmetricKeyProof: 1 = public_key, 2 = signature),
// 10000 = signed_header_data (SignedData: 1 = crx_id)。署名の検証そのものは Chrome が入れる時に行う。
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';

function varint(buf, pos) {
  let v = 0n;
  let shift = 0n;
  for (;;) {
    const b = buf[pos++];
    v |= BigInt(b & 0x7f) << shift;
    if (!(b & 0x80)) return [Number(v), pos];
    shift += 7n;
  }
}

/** 長さ付き (wire type 2) のフィールドだけ [field, bytes] で返す */
function* fields(buf) {
  let pos = 0;
  while (pos < buf.length) {
    let tag;
    [tag, pos] = varint(buf, pos);
    const wire = tag & 7;
    if (wire === 0) [, pos] = varint(buf, pos);
    else if (wire === 2) {
      let len;
      [len, pos] = varint(buf, pos);
      yield [tag >>> 3, buf.subarray(pos, pos + len)];
      pos += len;
    } else throw new Error(`扱えない wire type ${wire}`);
  }
}

export function crxId(crx) {
  if (crx.toString('latin1', 0, 4) !== 'Cr24' || crx.readUInt32LE(4) !== 3) throw new Error('CRX3 ではありません');
  const header = crx.subarray(12, 12 + crx.readUInt32LE(8));
  const ids = new Set();
  for (const [f, proof] of fields(header)) {
    if (f !== 2) continue;
    for (const [g, key] of fields(proof)) {
      if (g !== 1) continue;
      const hex = createHash('sha256').update(key).digest('hex').slice(0, 32);
      ids.add([...hex].map((c) => String.fromCharCode(97 + parseInt(c, 16))).join(''));
    }
  }
  if (ids.size !== 1) throw new Error(`RSA の鍵が ${ids.size} 個あります`);
  return [...ids][0];
}

const [file, expected] = process.argv.slice(2);
const id = crxId(readFileSync(file));
console.log(id);
if (expected && id !== expected) {
  console.error(`拡張 ID が違います: ${id} (期待 ${expected})。署名鍵 (Secret) を確かめてください`);
  process.exit(1);
}
