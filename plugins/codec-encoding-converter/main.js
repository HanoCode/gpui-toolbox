// Duck Toolbox 插件：编码转换（duck.codec.encoding-converter）。
//
// 契约：`handle(toolId, input)` 返回字符串结果；参数经 `duck.params` 注入。
//
// 与源实现（EncodingConverter.vue）的两处**有意**差异，都是为了让编解码真能
// 往返，而不是「编出来解不回去」：
//   1. `unicode` 解码同时认 `\uXXXX` 与 `\u{XXXXX}`——编码侧对辅助平面字符
//      就产出带花括号的形式，源实现的解码正则不认它，会把 emoji 解丢；
//   2. 八进制编码按 3 位补零（`\060` 这类），源实现的 `'0' + b.toString(8)`
//      在字节 < 8 时只有两位，解码按 3 位分组就会错位。

const DEFAULTS = { direction: "encode", encoding: "unicode" };

function takeParams() {
  const declared = (typeof duck.params === "object" && duck.params) || {};
  return { ...DEFAULTS, ...declared };
}

// ── UTF-8（沙箱没有 TextEncoder / TextDecoder，手写一遍） ────────────────

function utf8Bytes(text) {
  const bytes = [];
  for (const ch of Array.from(String(text))) {
    const cp = ch.codePointAt(0);
    if (cp < 0x80) {
      bytes.push(cp);
    } else if (cp < 0x800) {
      bytes.push(0xc0 | (cp >> 6), 0x80 | (cp & 0x3f));
    } else if (cp < 0x10000) {
      bytes.push(0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
    } else {
      bytes.push(
        0xf0 | (cp >> 18),
        0x80 | ((cp >> 12) & 0x3f),
        0x80 | ((cp >> 6) & 0x3f),
        0x80 | (cp & 0x3f),
      );
    }
  }
  return bytes;
}

function utf8Text(bytes) {
  let out = "";
  let i = 0;
  while (i < bytes.length) {
    const b = bytes[i];
    if (b < 0x80) {
      out += String.fromCharCode(b);
      i += 1;
      continue;
    }
    let need;
    let cp;
    if ((b & 0xe0) === 0xc0) {
      need = 1;
      cp = b & 0x1f;
    } else if ((b & 0xf0) === 0xe0) {
      need = 2;
      cp = b & 0x0f;
    } else if ((b & 0xf8) === 0xf0) {
      need = 3;
      cp = b & 0x07;
    } else {
      out += "\ufffd";
      i += 1;
      continue;
    }
    if (i + need >= bytes.length) {
      out += "\ufffd";
      break;
    }
    let ok = true;
    for (let k = 1; k <= need; k += 1) {
      const cont = bytes[i + k];
      if ((cont & 0xc0) !== 0x80) {
        ok = false;
        break;
      }
      cp = (cp << 6) | (cont & 0x3f);
    }
    if (!ok) {
      out += "\ufffd";
      i += 1;
      continue;
    }
    out += String.fromCodePoint(cp);
    i += need + 1;
  }
  return out;
}

function hex2(byte) {
  return byte.toString(16).toUpperCase().padStart(2, "0");
}

// ── Base32 / Base58（沙箱没有 BigInt，改走按位数组大数运算） ──────────────

const BASE32_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const BASE58_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

function base32Encode(bytes) {
  let bits = "";
  for (const byte of bytes) bits += byte.toString(2).padStart(8, "0");
  while (bits.length % 5 !== 0) bits += "0";
  let out = "";
  for (let i = 0; i < bits.length; i += 5) {
    out += BASE32_ALPHABET[parseInt(bits.slice(i, i + 5), 2)];
  }
  return out;
}

function base32Decode(text) {
  const cleaned = String(text).toUpperCase().replace(/[^A-Z2-7]/g, "");
  let bits = "";
  for (const ch of cleaned) {
    bits += BASE32_ALPHABET.indexOf(ch).toString(2).padStart(5, "0");
  }
  const bytes = [];
  for (let i = 0; i + 8 <= bits.length; i += 8) {
    bytes.push(parseInt(bits.slice(i, i + 8), 2));
  }
  return bytes;
}

function base58Encode(bytes) {
  let zeros = 0;
  while (zeros < bytes.length && bytes[zeros] === 0) zeros += 1;
  const digits = [];
  for (let i = zeros; i < bytes.length; i += 1) {
    let carry = bytes[i];
    for (let j = 0; j < digits.length; j += 1) {
      carry += digits[j] << 8;
      digits[j] = carry % 58;
      carry = (carry / 58) | 0;
    }
    while (carry > 0) {
      digits.push(carry % 58);
      carry = (carry / 58) | 0;
    }
  }
  let out = "1".repeat(zeros);
  for (let i = digits.length - 1; i >= 0; i -= 1) out += BASE58_ALPHABET[digits[i]];
  return out;
}

function base58Decode(text) {
  const cleaned = String(text).replace(/\s+/g, "");
  let zeros = 0;
  while (zeros < cleaned.length && cleaned[zeros] === "1") zeros += 1;
  const bytes = [];
  for (let i = zeros; i < cleaned.length; i += 1) {
    const value = BASE58_ALPHABET.indexOf(cleaned[i]);
    if (value < 0) throw new Error(duck.t("无效的 Base58 字符：{}", cleaned[i]));
    let carry = value;
    for (let j = 0; j < bytes.length; j += 1) {
      carry += bytes[j] * 58;
      bytes[j] = carry & 0xff;
      carry >>= 8;
    }
    while (carry > 0) {
      bytes.push(carry & 0xff);
      carry >>= 8;
    }
  }
  const out = [];
  for (let i = 0; i < zeros; i += 1) out.push(0);
  for (let i = bytes.length - 1; i >= 0; i -= 1) out.push(bytes[i]);
  return out;
}

// ── 编码 ────────────────────────────────────────────────────────────────

function encodeText(text, encoding) {
  const bytes = utf8Bytes(text);
  switch (encoding) {
    case "unicode":
      return Array.from(text)
        .map((ch) => {
          const cp = ch.codePointAt(0);
          return cp > 0xffff
            ? "\\u{" + cp.toString(16).toUpperCase() + "}"
            : "\\u" + cp.toString(16).toUpperCase().padStart(4, "0");
        })
        .join("");
    case "unicode_upper":
      return Array.from(text)
        .map((ch) => "\\U" + ch.codePointAt(0).toString(16).toUpperCase().padStart(8, "0"))
        .join("");
    case "html":
      return Array.from(text)
        .map((ch) => {
          const cp = ch.codePointAt(0);
          return cp > 0xffff ? "&#x" + cp.toString(16).toUpperCase() + ";" : "&#" + cp + ";";
        })
        .join("");
    case "url":
      return encodeURIComponent(text);
    case "hex":
      return bytes.map(hex2).join(" ");
    case "binary":
      return bytes.map((byte) => byte.toString(2).padStart(8, "0")).join(" ");
    case "octal":
      return bytes.map((byte) => byte.toString(8).padStart(3, "0")).join(" ");
    case "base32":
      return base32Encode(bytes);
    case "base58":
      return base58Encode(bytes);
    default:
      throw new Error(duck.t("未知编码：{}", encoding));
  }
}

// ── 解码 ────────────────────────────────────────────────────────────────

/** 把「数字码位 → 字符」的匹配结果拼回去；未命中的原文原样保留。
 *  `pattern` 的捕获组一律按 `radix` 进制解释（`&#x…` 与 `&#…` 进制不同，
 *  那条走下面的 `decodeHtmlEntities`，不共用这里）。 */
function replaceCodePoints(text, pattern, radix) {
  return String(text).replace(pattern, (...args) => {
    // args = [match, ...groups, offset, whole]（无命名组）
    const groups = args.slice(1, args.length - 2);
    const digits = groups.find((group) => typeof group === "string" && group !== "");
    if (!digits) return args[0];
    return String.fromCodePoint(parseInt(digits, radix));
  });
}

/** HTML 实体：`&#x1F600;` 走十六进制，`&#65;` 走十进制。 */
function decodeHtmlEntities(text) {
  return String(text).replace(/&#(?:x([0-9a-fA-F]+)|(\d+));/g, (match, hex, decimal) =>
    hex ? String.fromCodePoint(parseInt(hex, 16)) : String.fromCodePoint(parseInt(decimal, 10)),
  );
}

function decodeText(text, encoding) {
  switch (encoding) {
    case "unicode":
      return replaceCodePoints(text, /\\(?:u\{([0-9a-fA-F]+)\}|u([0-9a-fA-F]{4}))/g, 16);
    case "unicode_upper":
      return replaceCodePoints(text, /\\U([0-9a-fA-F]{8})/g, 16);
    case "html":
      return decodeHtmlEntities(text);
    case "url":
      try {
        return decodeURIComponent(text);
      } catch {
        throw new Error(duck.t("URL 解码失败：输入里有不完整或非法的 %XX 转义"));
      }
    case "hex": {
      const cleaned = String(text).replace(/[^0-9a-fA-F]/g, "");
      const bytes = [];
      for (let i = 0; i + 2 <= cleaned.length; i += 2) {
        bytes.push(parseInt(cleaned.slice(i, i + 2), 16));
      }
      return utf8Text(bytes);
    }
    case "binary": {
      const bits = String(text).replace(/[^01]/g, "");
      const bytes = [];
      for (let i = 0; i + 8 <= bits.length; i += 8) {
        bytes.push(parseInt(bits.slice(i, i + 8), 2));
      }
      return utf8Text(bytes);
    }
    case "octal": {
      const cleaned = String(text).replace(/[^0-7]/g, "");
      const bytes = [];
      for (let i = 0; i + 3 <= cleaned.length; i += 3) {
        bytes.push(parseInt(cleaned.slice(i, i + 3), 8));
      }
      return utf8Text(bytes);
    }
    case "base32":
      return utf8Text(base32Decode(text));
    case "base58":
      return utf8Text(base58Decode(text));
    default:
      throw new Error(duck.t("未知编码：{}", encoding));
  }
}

export function handle(_toolId, input) {
  const text = String(input || "");
  if (!text) throw new Error(duck.t("请输入要转换的内容"));
  const params = takeParams();
  const result =
    params.direction === "decode"
      ? decodeText(text, params.encoding)
      : encodeText(text, params.encoding);
  if (result === "") {
    throw new Error(duck.t("没有可转换的内容：检查输入是否与所选编码方案匹配"));
  }
  return result;
}
