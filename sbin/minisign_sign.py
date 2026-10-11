#!/usr/bin/env python3
"""给更新 manifest 签名：Ed25519 + [minisign] 线格式，**纯标准库**。

[minisign]: https://jedisct1.github.io/minisign/

用法：
    # 生成一对密钥（只做一次；私钥文件权限 0600，别进仓库）
    sbin/minisign_sign.py --generate-key ~/.duck-update-key

    # 签一份 manifest
    sbin/minisign_sign.py --sign-key ~/.duck-update-key.key --sign dist/update.json

    # 核对这份实现没跑偏
    sbin/minisign_sign.py --selftest

`manifest.py --sign-key <path>` 会调用本文件里的 `sign_bytes()`，所以正常发版不必
单独跑它；单独的命令行入口是给「已有 manifest、只想补签名」和排查用的。

# 为什么自己实现而不装 `minisign`

客户端只需要**验签**（`minisign-verify`，零传递依赖），而签名这一侧只在发布时用。
把它写成纯标准库，换来两件事：CI 的三个 runner 都自带 python3，不必装任何东西；
以及这条路径上没有一个我们看不透的二进制。代价是这一百来行数学要自己保证对——
所以有了 `--selftest`，它跑的是 RFC 8032 的官方测试向量。

# 线格式（与 `minisign-verify` 逐字节对齐）

**公钥**（`<name>.pub`，就是 minisign 的 `minisign.pub`）：

    第一行  untrusted comment: <任意文本>
    第二行  base64( "ED" + key_id[8] + public_key[32] )   # 42 字节

**签名**（`update.json.sig`）：

    第一行  untrusted comment: <任意文本>
    第二行  base64( "ED" + key_id[8] + signature[64] )    # 74 字节
    第三行  trusted comment: <文本>
    第四行  base64( global_signature[64] )

`"ED"` 表示**预哈希**（签名的是 BLAKE2b-512(内容)，不是内容本身）——新版 minisign
的默认，也是客户端唯一接受的那一种（`allow_legacy = false`）。写成 `"Ed"` 会被拒。

`global_signature` 签的是 `signature[64] || trusted comment 的正文`（**不含**
`"trusted comment: "` 这个前缀）。这一条是客户端源码里 `trusted_comment()` 的
切片行为决定的，写在这里免得日后有人"顺手"把前缀也签进去。

# 私钥文件格式

**不是** minisign 自带的那个（它默认用 scrypt 加密，我们需要的是 CI 里无人值守可用的
东西）。这里是一个我们自己定义的 JSON：

    {"comment": "...", "key_id": "<16 位十六进制>", "seed": "<base64 32 字节>"}

公钥与签名仍然是**标准 minisign 线格式**，所以客户端和任何第三方 minisign 工具都能验。
"""

import argparse
import base64
import hashlib
import json
import os
import secrets
import sys

# ---------------------------------------------------------------------------
# Ed25519（RFC 8032 §6 的参考实现，逐式照抄，只改写成现代 Python 的写法）
# ---------------------------------------------------------------------------

_P = 2**255 - 19
_L = 2**252 + 27742317777372353535851937790883648493
_D = -121665 * pow(121666, _P - 2, _P) % _P
_I = pow(2, (_P - 1) // 4, _P)

_BY = 4 * pow(5, _P - 2, _P) % _P
_BX = None  # 由 _x_recover 填，见下（B 的 x 坐标）
_B = None


def _sha512(data):
    return hashlib.sha512(data).digest()


def _bit(h, index):
    return (h[index // 8] >> (index % 8)) & 1


def _hash_to_int(h):
    return sum(2**i * _bit(h, i) for i in range(512))


def _x_recover(y):
    """从 y 还原曲线上的 x（取那个「偶」的根）。"""
    xx = (y * y - 1) * pow(_D * y * y + 1, _P - 2, _P)
    x = pow(xx, (_P + 3) // 8, _P)
    if (x * x - xx) % _P != 0:
        x = (x * _I) % _P
    if x % 2 != 0:
        x = _P - x
    return x


def _edwards_add(left, right):
    x1, y1 = left
    x2, y2 = right
    common = _D * x1 * x2 * y1 * y2
    x3 = (x1 * y2 + x2 * y1) * pow(1 + common, _P - 2, _P)
    y3 = (y1 * y2 + x1 * x2) * pow(1 - common, _P - 2, _P)
    return [x3 % _P, y3 % _P]


def _scalar_mult(point, scalar):
    if scalar == 0:
        return [0, 1]
    half = _scalar_mult(point, scalar // 2)
    doubled = _edwards_add(half, half)
    if scalar & 1:
        return _edwards_add(doubled, point)
    return doubled


def _encode_point(point):
    x, y = point
    bits = [(y >> i) & 1 for i in range(255)] + [x & 1]
    return bytes(sum(bits[i * 8 + j] << j for j in range(8)) for i in range(32))


def _base_point():
    global _BX, _B
    if _B is None:
        _BX = _x_recover(_BY)
        _B = [_BX % _P, _BY % _P]
    return _B


def _clamped_scalar(seed):
    """种子 → 私钥标量 a（RFC 8032 §5.1.5 的第一步）。"""
    h = _sha512(seed)
    return 2**254 + sum(2**i * _bit(h, i) for i in range(3, 254)), h


def public_key_of(seed):
    """32 字节种子 → 32 字节公钥。"""
    _require_seed(seed)
    a, _ = _clamped_scalar(seed)
    return _encode_point(_scalar_mult(_base_point(), a))


def sign(seed, message):
    """32 字节种子 + 任意字节 → 64 字节 Ed25519 签名。"""
    _require_seed(seed)
    a, h = _clamped_scalar(seed)
    public = _scalar_mult(_base_point(), a)
    nonce = _hash_to_int(_sha512(h[32:64] + message))
    r = _scalar_mult(_base_point(), nonce)
    challenge = _hash_to_int(
        _sha512(_encode_point(r) + _encode_point(public) + message)
    )
    s = (nonce + challenge * a) % _L
    return _encode_point(r) + s.to_bytes(32, "little")


def _require_seed(seed):
    if not isinstance(seed, (bytes, bytearray)) or len(seed) != 32:
        raise ValueError("an Ed25519 seed must be exactly 32 bytes")


# ---------------------------------------------------------------------------
# minisign 线格式
# ---------------------------------------------------------------------------

# "ED" = 预哈希（新版默认，也是客户端唯一接受的）；"Ed" = 旧版直签。
ALGORITHM_PREHASHED = b"ED"
PREHASH_DIGEST_SIZE = 64

# 客户端 `Signature::trusted_comment()` 会切掉的前缀。global_signature 签的是**切掉之后**
# 的正文，所以这里必须用同一个长度。
TRUSTED_PREFIX = "trusted comment: "


def prehash(message):
    """minisign 预哈希用的 BLAKE2b-512。"""
    return hashlib.blake2b(message, digest_size=PREHASH_DIGEST_SIZE).digest()


def public_key_record(key_id, public_key):
    """`minisign.pub` 的第二行（也是 `DUCK_UPDATE_PUBKEY` 要的那个值）。"""
    if len(key_id) != 8:
        raise ValueError("a minisign key id is 8 bytes")
    if len(public_key) != 32:
        raise ValueError("a minisign public key is 32 bytes")
    return base64.b64encode(
        ALGORITHM_PREHASHED + key_id + public_key
    ).decode("ascii")


def public_key_file(comment, key_id, public_key):
    """整个 `minisign.pub` 的内容。"""
    return "untrusted comment: {0}\n{1}\n".format(
        comment, public_key_record(key_id, public_key)
    )


def sign_bytes(seed, key_id, message, trusted_comment, comment="signature from duck update key"):
    """签一份内容，返回整个 `.sig` 文件的内容（四行，含结尾换行）。

    `trusted_comment` 是**正文**，不带 `"trusted comment: "` 前缀——前缀由这里补。
    """
    signature = sign(seed, prehash(message))
    global_signature = sign(
        seed, signature + trusted_comment.encode("utf-8")
    )
    payload = base64.b64encode(
        ALGORITHM_PREHASHED + key_id + signature
    ).decode("ascii")
    return (
        "untrusted comment: {0}\n{1}\ntrusted comment: {2}\n{3}\n".format(
            comment,
            payload,
            trusted_comment,
            base64.b64encode(global_signature).decode("ascii"),
        )
    )


# ---------------------------------------------------------------------------
# 我们自己的私钥文件（JSON）
# ---------------------------------------------------------------------------

KEY_FILE_VERSION = 1


def generate_key(comment="duck update signing key"):
    """造一对新密钥：返回 (私钥 JSON 字典, 公钥记录)。"""
    seed = secrets.token_bytes(32)
    # key_id 只是「这份签名是哪把钥匙签的」的标签，随机即可——minisign 自己也这么发。
    key_id = secrets.token_bytes(8)
    return (
        {
            "version": KEY_FILE_VERSION,
            "comment": comment,
            "key_id": key_id.hex(),
            "seed": base64.b64encode(seed).decode("ascii"),
        },
        public_key_record(key_id, public_key_of(seed)),
    )


def load_key(path):
    """读我们自己的私钥文件，返回 (seed, key_id)。"""
    with open(path, "rb") as handle:
        document = json.load(handle)
    try:
        seed = base64.b64decode(document["seed"], validate=True)
        key_id = bytes.fromhex(document["key_id"])
    except (KeyError, ValueError) as error:
        raise SystemExit("the key file {0} is unusable: {1}".format(path, error))
    if len(seed) != 32 or len(key_id) != 8:
        raise SystemExit(
            "the key file {0} does not hold a 32-byte seed and 8-byte key id".format(path)
        )
    return seed, key_id


# ---------------------------------------------------------------------------
# 自检：RFC 8032 的官方测试向量
# ---------------------------------------------------------------------------

# RFC 8032 §7.1 的三个向量。抄下来而不是「自己签自己验」，是因为自签自验对
# 一个系统性写错（比如少减一次模）的实现照样会过。
RFC8032_VECTORS = [
    (
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        "",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    ),
    (
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        "72",
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    ),
    (
        "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        "af82",
        "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    ),
]


def selftest():
    """跑 RFC 8032 向量 + 一次 minisign 封装往返。任何一条不过就抛。"""
    for index, (seed_hex, public_hex, message_hex, signature_hex) in enumerate(
        RFC8032_VECTORS
    ):
        seed = bytes.fromhex(seed_hex)
        message = b"" if not message_hex else bytes.fromhex(message_hex)
        expected_public = bytes.fromhex(public_hex)
        expected_signature = bytes.fromhex(signature_hex)

        actual_public = public_key_of(seed)
        if actual_public != expected_public:
            raise AssertionError(
                "vector {0}: public key mismatch\n  want {1}\n  got  {2}".format(
                    index, expected_public.hex(), actual_public.hex()
                )
            )
        actual_signature = sign(seed, message)
        if actual_signature != expected_signature:
            raise AssertionError(
                "vector {0}: signature mismatch\n  want {1}\n  got  {2}".format(
                    index, expected_signature.hex(), actual_signature.hex()
                )
            )

    # 封装层：结构对不对（长度、前缀、四行）。
    key_id = bytes(range(8))
    document, record = generate_key()
    seed = base64.b64decode(document["seed"])
    signature_text = sign_bytes(
        seed, key_id, b"the manifest bytes", "timestamp:0\tfile:update.json"
    )
    lines = signature_text.rstrip("\n").split("\n")
    if len(lines) != 4:
        raise AssertionError("a .sig is four lines, got {0}".format(len(lines)))
    if not lines[2].startswith(TRUSTED_PREFIX):
        raise AssertionError("the third line must carry the trusted comment")
    for line in (lines[1], lines[3]):
        raw = base64.b64decode(line, validate=True)
        if len(raw) not in (74, 64):
            raise AssertionError("unexpected base64 payload length {0}".format(len(raw)))
    if base64.b64decode(record, validate=True)[:2] != ALGORITHM_PREHASHED[:2]:
        raise AssertionError("the public key record must start with the algorithm tag")

    print("selftest ok: {0} RFC 8032 vectors + the minisign envelope".format(
        len(RFC8032_VECTORS)
    ))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--selftest", action="store_true", help="跑 RFC 8032 测试向量")
    parser.add_argument("--generate-key", metavar="PREFIX", help="生成 <PREFIX>.key 与 <PREFIX>.pub")
    parser.add_argument("--sign-key", metavar="KEY", help="私钥文件（我们自己的 JSON 格式）")
    parser.add_argument("--sign", metavar="FILE", help="要签的文件")
    parser.add_argument("--out", metavar="FILE", help="签名输出路径（默认 <FILE>.sig）")
    parser.add_argument(
        "--trusted-comment",
        metavar="TEXT",
        default="",
        help='写进 "trusted comment:" 的正文',
    )
    args = parser.parse_args(argv)

    if args.selftest:
        selftest()
        return 0

    if args.generate_key:
        document, record = generate_key()
        key_path = args.generate_key + ".key"
        pub_path = args.generate_key + ".pub"
        if os.path.exists(key_path):
            raise SystemExit("{0} already exists; refusing to overwrite a key".format(key_path))
        # 私钥先落盘再改权限：先建一个全世界读不到的文件，再往里写，中间没有窗口。
        descriptor = os.open(key_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w") as handle:
            json.dump(document, handle, indent=2)
            handle.write("\n")
        with open(pub_path, "w") as handle:
            handle.write(public_key_file(document["comment"], bytes.fromhex(document["key_id"]),
                                         public_key_of(base64.b64decode(document["seed"]))))
        print("wrote {0} (private) and {1} (public)".format(key_path, pub_path))
        print("public key for DUCK_UPDATE_PUBKEY:\n{0}".format(record))
        return 0

    if args.sign:
        if not args.sign_key:
            raise SystemExit("--sign needs --sign-key")
        seed, key_id = load_key(args.sign_key)
        with open(args.sign, "rb") as handle:
            message = handle.read()
        signature = sign_bytes(seed, key_id, message, args.trusted_comment)
        out = args.out or (args.sign + ".sig")
        with open(out, "w") as handle:
            handle.write(signature)
        print("wrote {0}".format(out))
        return 0

    parser.print_help()
    return 1


if __name__ == "__main__":
    sys.exit(main())
