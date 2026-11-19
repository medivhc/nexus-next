//! 短加入码（R62）：生成、解析，从短码派生碰头服务上的位置和加密密钥，加密放到碰头服务上的数据。
//!
//! 短码是 10 个 Crockford base32 字符（50 位随机）。碰头服务上的记录用从短码派生的签名密钥签名，
//! 内容用派生的加密密钥加密：不知道短码的人看得到记录也得不到任何东西。派生用 Argon2id，
//! 故意做得慢，让从公开数据反推短码不可行。

/// 短码的字符数（不含分隔符）。
pub const SHORT_CODE_LEN: usize = 10;

/// Crockford base32：去掉了容易混淆的 I、L、O、U。
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 派生的参数（FP-6.1-04 固定这组值）：Argon2id、19 MiB、2 轮、1 条并行。
pub const ARGON2_MEMORY_KIB: u32 = 19 * 1024;
pub const ARGON2_ITERATIONS: u32 = 2;
pub const ARGON2_PARALLELISM: u32 = 1;
const ARGON2_SALT: &[u8] = b"nexus/join/short-code/v1";

/// 规范化之后的短码：10 个 Crockford 字符，大写。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortCode(String);

/// 从短码派生的两把密钥。
#[derive(Clone, PartialEq, Eq)]
pub struct RendezvousKeys {
    /// 签名密钥的种子：它的公钥是碰头服务上的位置。
    pub seed: [u8; 32],
    /// 加密放到碰头服务上的数据。
    pub enc: [u8; 32],
}

impl core::fmt::Debug for RendezvousKeys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RendezvousKeys(..)")
    }
}

impl ShortCode {
    /// 用 7 个随机字节生成（取前 50 位）。
    pub fn from_random(bytes: [u8; 7]) -> Self {
        let mut v: u64 = 0;
        for b in bytes {
            v = (v << 8) | u64::from(b);
        }
        v >>= 56 - 5 * SHORT_CODE_LEN as u32;
        let s = (0..SHORT_CODE_LEN)
            .rev()
            .map(|i| CROCKFORD[((v >> (5 * i)) & 31) as usize] as char)
            .collect();
        ShortCode(s)
    }

    /// 解析用户输入：不区分大小写，忽略 `-`、空格；O 当作 0，I、L 当作 1。
    pub fn parse(input: &str) -> Option<Self> {
        let mut s = String::with_capacity(SHORT_CODE_LEN);
        for c in input.chars() {
            if c == '-' || c.is_whitespace() {
                continue;
            }
            let c = match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            };
            if !c.is_ascii() || !CROCKFORD.contains(&(c as u8)) {
                return None;
            }
            s.push(c);
        }
        (s.len() == SHORT_CODE_LEN).then_some(ShortCode(s))
    }

    /// 规范的 10 个字符。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 显示给人看的形式：`XXXXX-XXXXX`。
    pub fn display(&self) -> String {
        format!("{}-{}", &self.0[..5], &self.0[5..])
    }

    /// 派生碰头服务上的位置和加密密钥。很慢（几十毫秒），每次加入只做一两次。
    pub fn keys(&self) -> RendezvousKeys {
        use argon2::{Algorithm, Argon2, Params, Version};
        let params = Params::new(ARGON2_MEMORY_KIB, ARGON2_ITERATIONS, ARGON2_PARALLELISM, Some(64)).expect("参数固定且有效");
        let mut out = [0u8; 64];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(self.0.as_bytes(), ARGON2_SALT, &mut out)
            .expect("输出长度固定且有效");
        let mut keys = RendezvousKeys { seed: [0; 32], enc: [0; 32] };
        keys.seed.copy_from_slice(&out[..32]);
        keys.enc.copy_from_slice(&out[32..]);
        keys
    }
}

const SALT_LEN: usize = 16;
const TAG_LEN: usize = 16;

fn keystream(keys: &RendezvousKeys, salt: &[u8], len: usize) -> Vec<u8> {
    let mut h = blake3::Hasher::new_keyed(&keys.enc);
    h.update(b"stream");
    h.update(salt);
    let mut out = vec![0u8; len];
    h.finalize_xof().fill(&mut out);
    out
}

fn tag(keys: &RendezvousKeys, salt: &[u8], ct: &[u8]) -> [u8; TAG_LEN] {
    let mut h = blake3::Hasher::new_keyed(&keys.enc);
    h.update(b"tag");
    h.update(salt);
    h.update(ct);
    let mut t = [0u8; TAG_LEN];
    t.copy_from_slice(&h.finalize().as_bytes()[..TAG_LEN]);
    t
}

/// 加密要放到碰头服务上的数据：`盐 ‖ 密文 ‖ 校验`。盐由调用方给出（随机）。
pub fn seal(keys: &RendezvousKeys, salt: [u8; SALT_LEN], plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(SALT_LEN + plain.len() + TAG_LEN);
    out.extend_from_slice(&salt);
    out.extend(plain.iter().zip(keystream(keys, &salt, plain.len())).map(|(a, b)| a ^ b));
    let t = tag(keys, &salt, &out[SALT_LEN..]);
    out.extend_from_slice(&t);
    out
}

/// 解密；密钥不对或数据被改过时为 None。
pub fn open(keys: &RendezvousKeys, sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < SALT_LEN + TAG_LEN {
        return None;
    }
    let (salt, rest) = sealed.split_at(SALT_LEN);
    let (ct, t) = rest.split_at(rest.len() - TAG_LEN);
    // 比较的时间和内容无关。
    let expect = tag(keys, salt, ct);
    if expect.iter().zip(t).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return None;
    }
    Some(ct.iter().zip(keystream(keys, salt, ct.len())).map(|(a, b)| a ^ b).collect())
}

/// 一段数据的校验值（长加入码末尾用）。
pub fn checksum(bytes: &[u8]) -> [u8; 32] {
    *blake3::Hasher::new_derive_key("nexus/join/long-code/check/v1").update(bytes).finalize().as_bytes()
}

/// 小写的 RFC 4648 base32（不补等号），长加入码用（R63）。
pub fn base32_encode(bytes: &[u8]) -> String {
    const A: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(A[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(A[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// [`base32_encode`] 的逆；不区分大小写。多余的位不是零、或有非法字符时为 None。
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c.to_ascii_lowercase() {
            c @ b'a'..=b'z' => c - b'a',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // 剩下不到一个字节的位必须是零（同一段字节只有一种写法）。
    if bits >= 5 || acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(s: &str) -> ShortCode {
        ShortCode::parse(s).unwrap()
    }

    /// FP-6.1-01：10 个字符、两组、只含 Crockford 字符。
    #[test]
    fn generated_codes_are_ten_crockford_chars() {
        for seed in 0u8..50 {
            let c = ShortCode::from_random([seed, seed.wrapping_mul(7), 3, 200, seed ^ 0x5a, 9, 255]);
            assert_eq!(c.as_str().len(), SHORT_CODE_LEN);
            assert!(c.as_str().bytes().all(|b| CROCKFORD.contains(&b)), "{c:?}");
            let d = c.display();
            assert_eq!(d.len(), 11);
            assert_eq!(&d[5..6], "-");
            assert_eq!(ShortCode::parse(&d), Some(c));
        }
        // 50 位都用上：只差最后一个有效位的输入得到不同的短码。
        assert_ne!(ShortCode::from_random([0, 0, 0, 0, 0, 0, 0x40]), ShortCode::from_random([0; 7]));
        assert_eq!(ShortCode::from_random([0, 0, 0, 0, 0, 0, 0x3f]), ShortCode::from_random([0; 7]));
    }

    /// FP-6.1-02：输入宽松。
    #[test]
    fn parsing_is_forgiving() {
        let c = code("7KQ4M-9XZ21");
        for s in ["7kq4m9xz21", " 7KQ4M 9XZ21 ", "7kq4m-9xz2l", "7kq4m-9xz2I", "7-K-Q-4-M-9-X-Z-2-1"] {
            assert_eq!(ShortCode::parse(s).as_ref(), Some(&c), "{s}");
        }
        assert_eq!(code("0O0O0-00000"), code("00000-00000"));
        for bad in ["", "7KQ4M-9XZ2", "7KQ4M-9XZ211", "7KQ4M-9XZ2U", "7KQ4M_9XZ21", "７KQ4M-9XZ21", "nxj2-abc"] {
            assert_eq!(ShortCode::parse(bad), None, "{bad}");
        }
    }

    /// FP-6.1-04：派生的参数固定；不同短码得到不同的位置和密钥；同一个短码总得到同样的结果。
    #[test]
    fn derivation_is_fixed_and_distinct() {
        assert_eq!((ARGON2_MEMORY_KIB, ARGON2_ITERATIONS, ARGON2_PARALLELISM), (19456, 2, 1));
        let a = code("7KQ4M-9XZ21").keys();
        assert_eq!(a, code("7kq4m9xz2l").keys());
        let b = code("7KQ4M-9XZ22").keys();
        assert_ne!(a.seed, b.seed);
        assert_ne!(a.enc, b.enc);
        assert_ne!(a.seed, a.enc);
    }

    /// FP-6.1-04：密文里没有明文的片段；错误的短码、被改过的数据都打不开。
    #[test]
    fn sealed_data_reveals_nothing_without_the_code() {
        let keys = code("7KQ4M-9XZ21").keys();
        let plain: Vec<u8> = (0..120u8).collect();
        let sealed = seal(&keys, [9; 16], &plain);
        assert_eq!(open(&keys, &sealed).as_deref(), Some(&plain[..]));
        for w in plain.windows(4) {
            assert!(!sealed[16..sealed.len() - 16].windows(4).any(|x| x == w), "密文里出现了明文片段 {w:?}");
        }
        assert_eq!(open(&code("7KQ4M-9XZ22").keys(), &sealed), None);
        for i in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[i] ^= 1;
            assert_eq!(open(&keys, &bad), None, "改了第 {i} 个字节");
        }
        assert_eq!(open(&keys, &sealed[..20]), None);
        // 同样的明文、不同的盐：密文不同。
        assert_ne!(seal(&keys, [1; 16], &plain), sealed);
    }

    #[test]
    fn base32_round_trips() {
        for n in 0..40usize {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            let s = base32_encode(&bytes);
            assert!(s.bytes().all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)));
            assert_eq!(base32_decode(&s), Some(bytes.clone()));
            assert_eq!(base32_decode(&s.to_uppercase()), Some(bytes));
        }
        assert_eq!(base32_encode(b"foobar"), "mzxw6ytboi");
        assert_eq!(base32_decode("mzxw6ytboj"), None); // 多余的位不是零
        assert_eq!(base32_decode("mzxw6ytbo1"), None);
    }
}
