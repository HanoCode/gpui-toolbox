//! 随机文本生成：**六类字段**（姓名 / 地址 / 网络 / 日期时间 / 数字 / 客户信息）。
//!
//! 逐条移植参考仓 `src/tools/RandomTextGenerator.vue`（1,417 行，纯前端）：
//!
//! | 参考页 | 这里 |
//! |---|---|
//! | `generateZhName` / `generateEnName` / `generateJaName` / `generateCompany` | [`names`] |
//! | `generateZhAddress` / `generateUsAddress` | [`addresses`] |
//! | `generateEmail` … `generateUUID`（9 种） | [`internet`] |
//! | `generateDateValue`（7 种） | [`datetime`] |
//! | `generateNumber`（9 种） | [`numbers`] |
//! | `generateCustomer`（25 国 + 机构树） | [`customer`] |
//!
//! ## 两处必须知道的「照抄」
//!
//! 1. **单次与批量的语义不同**。参考页有两个按钮：「生成」与「批量生成」，同一个
//!    类型走的是两段代码——例如「日期」单次返回**今天**，批量返回 2000-01-01 至今的
//!    随机日期；「时间」单次是**此刻**，批量是随机时刻。这里不统一它：`count == 1`
//!    走单次口径、`count > 1` 走批量口径，与用户原来的两个按钮一一对应。
//! 2. **共用的素材词表在 [`crate::datagen_tables`]**，那些中文是生成结果本身
//!    （姓名、地址、账户类型），不是界面文案，所以不进译文表。
//!
//! 内核**不产出**整句文案：客户信息报告的中文标签由插件层（`duck-files-tool`）渲染，
//! 这样英文界面下看到的是英文标签；本模块只给结构化的 [`Customer`]。
//!
//! 随机源是 `getrandom` 取一次系统熵做种、之后走 SplitMix64：一次调用要生成几十条
//! 数据，每条都去读 `/dev/urandom` 没必要，而固定种子（[`Rng::from_seed`]）让测试
//! 可以断言具体输出。

use chrono::{Datelike, Local, Timelike};

use crate::datagen_tables as tables;

/// SplitMix64：一个 64 位状态、无外部依赖、对「造测试数据」这个用途足够均匀。
pub struct Rng {
    state: u64,
}

impl Rng {
    /// 系统熵做种（一次 `getrandom`）。取不到熵时退回时间戳——测试数据不该因此失败。
    pub fn seeded() -> Self {
        let mut bytes = [0u8; 8];
        let state = match getrandom::fill(&mut bytes) {
            Ok(()) => u64::from_le_bytes(bytes),
            Err(_) => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0x9E37_79B9_7F4A_7C15, |elapsed| elapsed.as_nanos() as u64),
        };
        Self { state }
    }

    /// 固定种子（测试用）。
    pub fn from_seed(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `[0, bound)`。`bound == 0` 返回 0（调用点不会传 0，但不 panic 更稳妥）。
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        self.next_u64() % bound
    }

    /// 闭区间 `[min, max]` 的整数（参考页 `randomInt`）。
    pub fn int(&mut self, min: i64, max: i64) -> i64 {
        if max <= min {
            return min;
        }
        min + self.below((max - min + 1) as u64) as i64
    }

    /// `[0, 1)` 的浮点（参考页 `Math.random()`）。
    pub fn float(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// 参考页的 `Math.random() > p`（注意是**大于**，不是小于）。
    pub fn more_than(&mut self, p: f64) -> bool {
        self.float() > p
    }

    /// 从切片里等概率取一个（参考页 `randomChoice`）。
    pub fn choice<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    /// 生成 `count` 个字符（参考页 `for (i < len) s += pool[...]`）。
    fn string_of(&mut self, pool: &[char], count: usize) -> String {
        (0..count).map(|_| *self.choice(pool)).collect()
    }

    /// 从 ASCII 字符集里取 `[min, max]` 个字符。
    fn ascii_of(&mut self, charset: &str, min: i64, max: i64) -> String {
        let chars: Vec<char> = charset.chars().collect();
        let count = self.int(min, max) as usize;
        (0..count).map(|_| *self.choice(&chars)).collect()
    }
}

// ══════════════════════════════════════════════════════════════════════
// 姓名
// ══════════════════════════════════════════════════════════════════════

/// 姓名语种（参考页 `lang`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
    Ja,
}

impl Lang {
    pub fn parse(value: &str) -> Self {
        match value {
            "en" => Self::En,
            "ja" => Self::Ja,
            _ => Self::Zh,
        }
    }
}

/// 性别倾向（参考页 `gender`，空串 = 不限）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gender {
    Any,
    Male,
    Female,
}

impl Gender {
    pub fn parse(value: &str) -> Self {
        match value {
            "male" => Self::Male,
            "female" => Self::Female,
            _ => Self::Any,
        }
    }
}

/// 姓名生成选项。
#[derive(Debug, Clone, Copy)]
pub struct NameOptions {
    pub lang: Lang,
    pub gender: Gender,
    /// 同时生成一个公司名（参考页 `withCompany`）。
    pub with_company: bool,
}

impl Default for NameOptions {
    fn default() -> Self {
        Self {
            lang: Lang::Zh,
            gender: Gender::Any,
            with_company: false,
        }
    }
}

fn zh_given(rng: &mut Rng, gender: Gender) -> &'static str {
    match gender {
        Gender::Male => rng.choice(tables::ZH_GIVEN_MALE),
        Gender::Female => rng.choice(tables::ZH_GIVEN_FEMALE),
        Gender::Any => {
            // 参考页把三个池拼起来再取（`[...MALE, ...FEMALE, ...NEUTRAL]`）。
            let pool_len = tables::ZH_GIVEN_MALE.len()
                + tables::ZH_GIVEN_FEMALE.len()
                + tables::ZH_GIVEN_NEUTRAL.len();
            let index = rng.below(pool_len as u64) as usize;
            let male = tables::ZH_GIVEN_MALE.len();
            let female = tables::ZH_GIVEN_FEMALE.len();
            if index < male {
                tables::ZH_GIVEN_MALE[index]
            } else if index < male + female {
                tables::ZH_GIVEN_FEMALE[index - male]
            } else {
                tables::ZH_GIVEN_NEUTRAL[index - male - female]
            }
        }
    }
}

/// 中文姓名（`generateZhName`）。
pub fn zh_name(rng: &mut Rng, gender: Gender) -> String {
    let surname = *rng.choice(tables::ZH_SURNAMES);
    let mut given = zh_given(rng, gender).to_owned();
    if rng.more_than(0.5) {
        given.push_str(zh_given(rng, gender));
    }
    format!("{surname}{given}")
}

/// 英文姓名（`generateEnName`）：`First Last`，首字母大写。
pub fn en_name(rng: &mut Rng) -> String {
    let first = format!(
        "{}{}",
        rng.choice(tables::EN_FIRST_PREFIX),
        rng.choice(tables::EN_FIRST_SUFFIX)
    );
    let last = format!(
        "{}{}",
        rng.choice(tables::EN_LAST_PREFIX),
        rng.choice(tables::EN_LAST_SUFFIX)
    );
    format!("{} {}", capitalize(&first), capitalize(&last))
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => {
            let mut out = first.to_uppercase().collect::<String>();
            out.push_str(chars.as_str());
            out
        }
        None => String::new(),
    }
}

/// 日文姓名（`generateJaName`）：`姓 名`。
pub fn ja_name(rng: &mut Rng) -> String {
    let surname = *rng.choice(tables::JA_SURNAME);
    let first = *rng.choice(tables::JA_GIVEN_FIRST);
    let ending = *rng.choice(tables::JA_GIVEN_ENDING);
    format!("{surname} {first}{ending}")
}

/// 公司名（`generateCompany`）。
pub fn company(rng: &mut Rng) -> String {
    format!(
        "{}{}{}",
        rng.choice(tables::COMPANY_PREFIX),
        rng.choice(tables::COMPANY_TYPE),
        rng.choice(tables::COMPANY_SUFFIX)
    )
}

/// 生成 `count` 条姓名。`count == 1` 时公司名换行拼，批量时用 ` | ` 分隔（参考页两种按钮）。
pub fn names(rng: &mut Rng, options: NameOptions, count: usize) -> Vec<String> {
    let single = count == 1;
    (0..count)
        .map(|_| {
            let base = match options.lang {
                Lang::Zh => zh_name(rng, options.gender),
                Lang::En => en_name(rng),
                Lang::Ja => ja_name(rng),
            };
            if options.with_company {
                let separator = if single { "\n" } else { " | " };
                format!("{base}{separator}{}", company(rng))
            } else {
                base
            }
        })
        .collect()
}

// ══════════════════════════════════════════════════════════════════════
// 地址
// ══════════════════════════════════════════════════════════════════════

/// 地址国家（参考页 `country`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressCountry {
    Zh,
    Us,
}

impl AddressCountry {
    pub fn parse(value: &str) -> Self {
        if value == "us" { Self::Us } else { Self::Zh }
    }
}

/// 地址生成选项。
#[derive(Debug, Clone, Copy)]
pub struct AddressOptions {
    pub country: AddressCountry,
    pub with_postal_code: bool,
    pub with_coordinate: bool,
}

impl Default for AddressOptions {
    fn default() -> Self {
        Self {
            country: AddressCountry::Zh,
            with_postal_code: true,
            with_coordinate: false,
        }
    }
}

/// 中国地址（`generateZhAddress`）：省 + 随机市/县/区 + 街道 + 门牌号 [+ 邮编] [+ 坐标]。
pub fn zh_address(rng: &mut Rng, options: AddressOptions) -> String {
    let province = *rng.choice(tables::ZH_PROVINCES);
    let city_len = rng.int(2, 4) as usize;
    let mut city = rng.string_of(tables::ZH_CITY_HANZI, city_len);
    city.push_str(rng.choice(tables::ZH_CITY_SUFFIX));
    let street = format!(
        "{}{}",
        rng.choice(tables::ZH_STREET_PREFIX),
        rng.choice(tables::ZH_STREET_SUFFIX)
    );
    let number = rng.int(1, 999);
    let mut address = format!(
        "{province}{city}{street}{number}{}",
        tables::ZH_NUMBER_SUFFIX
    );
    if options.with_postal_code {
        let first = rng.int(1, 9);
        let rest = rng.int(10000, 99999);
        address.push_str(&format!("{}{first}{rest}", tables::ZH_POSTCODE_PREFIX));
    }
    if options.with_coordinate {
        let lat = rng.int(1800, 5400) as f64 / 100.0;
        let lng = rng.int(7300, 13500) as f64 / 100.0;
        address.push_str(&format!(" ({lat:.2}, {lng:.2})"));
    }
    address
}

/// 美国地址（`generateUsAddress`）：`门牌 街道, 城市, 州 邮编` [+ 坐标]。
///
/// 邮编在美国地址里是**无条件**带的（参考页没有对应勾选框），照抄。
pub fn us_address(rng: &mut Rng, with_coordinate: bool) -> String {
    let number = rng.int(1, 9999);
    let street = *rng.choice(tables::US_STREETS);
    let city = *rng.choice(tables::US_CITIES);
    let state = *rng.choice(tables::US_STATES);
    let zip = rng.int(10000, 99999);
    let mut address = format!("{number} {street}, {city}, {state} {zip}");
    if with_coordinate {
        let lat = rng.int(-9000, 4900) as f64 / 100.0;
        let lng = rng.int(-12400, -6600) as f64 / 100.0;
        address.push_str(&format!(" ({lat:.2}, {lng:.2})"));
    }
    address
}

/// 生成 `count` 条地址。
pub fn addresses(rng: &mut Rng, options: AddressOptions, count: usize) -> Vec<String> {
    (0..count)
        .map(|_| match options.country {
            AddressCountry::Zh => zh_address(rng, options),
            AddressCountry::Us => us_address(rng, options.with_coordinate),
        })
        .collect()
}

// ══════════════════════════════════════════════════════════════════════
// 网络
// ══════════════════════════════════════════════════════════════════════

/// 网络类字段（参考页 `internetTypes`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternetKind {
    Email,
    Url,
    Ipv4,
    Ipv6,
    Mac,
    Username,
    Password,
    Domain,
    Uuid,
}

impl InternetKind {
    pub fn parse(value: &str) -> Self {
        match value {
            "url" => Self::Url,
            "ipv4" => Self::Ipv4,
            "ipv6" => Self::Ipv6,
            "mac" => Self::Mac,
            "username" => Self::Username,
            "password" => Self::Password,
            "domain" => Self::Domain,
            "uuid" => Self::Uuid,
            _ => Self::Email,
        }
    }
}

fn email(rng: &mut Rng) -> String {
    let username = rng.ascii_of("abcdefghijklmnopqrstuvwxyz0123456789", 5, 15);
    format!("{username}@{}", rng.choice(tables::EMAIL_DOMAINS))
}

fn url(rng: &mut Rng) -> String {
    let protocol = if rng.more_than(0.3) { "https" } else { "http" };
    let domain = rng.ascii_of("abcdefghijklmnopqrstuvwxyz", 5, 12);
    let tld = rng.choice(tables::TLDS);
    let path_count = rng.int(0, 3);
    let mut path = String::new();
    for _ in 0..path_count {
        path.push('/');
        path.push_str(&rng.ascii_of("abcdefghijklmnopqrstuvwxyz", 3, 8));
    }
    format!("{protocol}://{domain}.{tld}{path}")
}

fn ipv4(rng: &mut Rng) -> String {
    format!(
        "{}.{}.{}.{}",
        rng.int(1, 255),
        rng.int(0, 255),
        rng.int(0, 255),
        rng.int(1, 254)
    )
}

fn ipv6(rng: &mut Rng) -> String {
    (0..8)
        .map(|_| format!("{:04x}", rng.int(0, 65535)))
        .collect::<Vec<_>>()
        .join(":")
}

fn mac(rng: &mut Rng) -> String {
    (0..6)
        .map(|_| format!("{:02x}", rng.int(0, 255)))
        .collect::<Vec<_>>()
        .join(":")
}

fn username(rng: &mut Rng) -> String {
    rng.ascii_of("abcdefghijklmnopqrstuvwxyz0123456789_", 6, 16)
}

fn password(rng: &mut Rng) -> String {
    rng.ascii_of(
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*",
        12,
        20,
    )
}

fn domain(rng: &mut Rng) -> String {
    let name = rng.ascii_of("abcdefghijklmnopqrstuvwxyz", 5, 12);
    format!("{name}.{}", rng.choice(tables::TLDS))
}

fn uuid(rng: &mut Rng) -> String {
    let hex: Vec<char> = "0123456789abcdef".chars().collect();
    let mut out = String::with_capacity(36);
    for (index, width) in [8usize, 4, 4, 4, 12].into_iter().enumerate() {
        for _ in 0..width {
            out.push(*rng.choice(&hex));
        }
        if index < 4 {
            out.push('-');
        }
    }
    out
}

/// 生成一个网络类字段（`generateInternet` 的 `switch`）。
pub fn internet_one(rng: &mut Rng, kind: InternetKind) -> String {
    match kind {
        InternetKind::Email => email(rng),
        InternetKind::Url => url(rng),
        InternetKind::Ipv4 => ipv4(rng),
        InternetKind::Ipv6 => ipv6(rng),
        InternetKind::Mac => mac(rng),
        InternetKind::Username => username(rng),
        InternetKind::Password => password(rng),
        InternetKind::Domain => domain(rng),
        InternetKind::Uuid => uuid(rng),
    }
}

/// 生成 `count` 条网络类字段。
pub fn internet(rng: &mut Rng, kind: InternetKind, count: usize) -> Vec<String> {
    (0..count).map(|_| internet_one(rng, kind)).collect()
}

// ══════════════════════════════════════════════════════════════════════
// 日期时间
// ══════════════════════════════════════════════════════════════════════

/// 日期时间类字段（参考页 `datetimeTypes`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTimeKind {
    Date,
    Time,
    DateTime,
    Timestamp,
    Weekday,
    Birthday,
    Birthdate,
}

impl DateTimeKind {
    pub fn parse(value: &str) -> Self {
        match value {
            "time" => Self::Time,
            "datetime" => Self::DateTime,
            "timestamp" => Self::Timestamp,
            "weekday" => Self::Weekday,
            "birthday" => Self::Birthday,
            "birthdate" => Self::Birthdate,
            _ => Self::Date,
        }
    }
}

/// 公历某年某月某日（`chrono` 的 `NaiveDate` 等价物，但只留需要的三个字段）。
struct Civil {
    year: i32,
    month: u32,
    day: u32,
}

impl Civil {
    fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    fn datetime(&self, hour: u32, minute: u32, second: u32) -> String {
        format!("{} {hour:02}:{minute:02}:{second:02}", self.date())
    }
}

fn now_civil() -> (Civil, u32, u32, u32, u32) {
    let now = Local::now();
    (
        Civil {
            year: now.year(),
            month: now.month(),
            day: now.day(),
        },
        now.hour(),
        now.minute(),
        now.second(),
        now.weekday().num_days_from_sunday(),
    )
}

/// 2000-01-01 至今的随机日期（批量口径的起点）。
fn random_civil(rng: &mut Rng, start_year: i32) -> Civil {
    let (today, _, _, _, _) = now_civil();
    let start = chrono::NaiveDate::from_ymd_opt(start_year, 1, 1).expect("valid date");
    let end = chrono::NaiveDate::from_ymd_opt(today.year, today.month, today.day)
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(2000, 1, 1).expect("fallback date"));
    let span = (end - start).num_days().max(1);
    let offset = rng.int(0, span);
    let picked = start + chrono::Duration::days(offset);
    Civil {
        year: picked.year(),
        month: picked.month(),
        day: picked.day(),
    }
}

/// 生成一个日期时间类字段。
///
/// `batch` 为真时用参考页「批量生成」那套随机口径；为假时用「生成」那套
/// （今天是今天、此刻是此刻）。调用方按 `count == 1` 决定。
pub fn datetime_one(rng: &mut Rng, kind: DateTimeKind, batch: bool) -> String {
    let (today, hour, minute, second, weekday) = now_civil();
    match kind {
        DateTimeKind::Date if batch => random_civil(rng, 2000).date(),
        DateTimeKind::Date => today.date(),
        DateTimeKind::Time if batch => format!(
            "{:02}:{:02}:{:02}",
            rng.int(0, 23),
            rng.int(0, 59),
            rng.int(0, 59)
        ),
        DateTimeKind::Time => format!("{hour:02}:{minute:02}:{second:02}"),
        DateTimeKind::DateTime if batch => {
            let date = random_civil(rng, 2000);
            date.datetime(
                rng.int(0, 23) as u32,
                rng.int(0, 59) as u32,
                rng.int(0, 59) as u32,
            )
        }
        DateTimeKind::DateTime => {
            let date = random_civil(rng, today.year);
            date.datetime(hour, minute, second)
        }
        DateTimeKind::Timestamp if batch => {
            let date = random_civil(rng, 2000);
            let stamp = chrono::NaiveDate::from_ymd_opt(date.year, date.month, date.day)
                .and_then(|day| day.and_hms_opt(0, 0, 0))
                .map_or(0, |moment| moment.and_utc().timestamp());
            (stamp + rng.int(0, 86_399)).to_string()
        }
        DateTimeKind::Timestamp => Local::now().timestamp().to_string(),
        DateTimeKind::Weekday if batch => (*rng.choice(tables::WEEKDAYS)).to_owned(),
        DateTimeKind::Weekday => tables::WEEKDAYS[weekday as usize].to_owned(),
        DateTimeKind::Birthday => {
            let age = rng.int(18, 80);
            format!(
                "{:04}-{:02}-{:02}",
                today.year - age as i32,
                rng.int(1, 12),
                rng.int(1, 28)
            )
        }
        DateTimeKind::Birthdate => format!(
            "{:04}-{:02}-{:02}",
            rng.int(1950, 2005),
            rng.int(1, 12),
            rng.int(1, 28)
        ),
    }
}

/// 生成 `count` 条日期时间字段。
pub fn datetime(rng: &mut Rng, kind: DateTimeKind, count: usize) -> Vec<String> {
    let batch = count > 1;
    (0..count).map(|_| datetime_one(rng, kind, batch)).collect()
}

// ══════════════════════════════════════════════════════════════════════
// 数字
// ══════════════════════════════════════════════════════════════════════

/// 数字类字段（参考页 `numberTypes`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberKind {
    Phone,
    IdCard,
    BankCard,
    Age,
    AdultAge,
    Integer,
    Float,
    Price,
    Percentage,
}

impl NumberKind {
    pub fn parse(value: &str) -> Self {
        match value {
            "idcard" => Self::IdCard,
            "bankcard" => Self::BankCard,
            "age" => Self::Age,
            "adult_age" => Self::AdultAge,
            "integer" => Self::Integer,
            "float" => Self::Float,
            "price" => Self::Price,
            "percentage" => Self::Percentage,
            _ => Self::Phone,
        }
    }
}

/// 身份证第 18 位（`calculateIdCheckDigit`）：GB 11643 的加权模 11。
pub fn id_check_digit(first17: &str) -> char {
    const WEIGHTS: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const CODES: [char; 11] = ['1', '0', 'X', '9', '8', '7', '6', '5', '4', '3', '2'];
    let mut sum = 0u32;
    for (index, digit) in first17.chars().take(17).enumerate() {
        sum += digit.to_digit(10).unwrap_or(0) * WEIGHTS[index];
    }
    CODES[(sum % 11) as usize]
}

/// 18 位身份证（`generateIdCard`）。
pub fn id_card(rng: &mut Rng) -> String {
    let area = *rng.choice(tables::AREA_CODES);
    let year = rng.int(1950, 2005);
    let month = rng.int(1, 12);
    let day = rng.int(1, 28);
    let sequence = rng.int(1, 999);
    let first17 = format!("{area}{year:04}{month:02}{day:02}{sequence:03}");
    let check = id_check_digit(&first17);
    format!("{first17}{check}")
}

fn phone(rng: &mut Rng) -> String {
    format!(
        "{}{}",
        rng.choice(tables::PHONE_PREFIXES),
        rng.int(10_000_000, 99_999_999)
    )
}

fn bank_card(rng: &mut Rng) -> String {
    let mut card = (*rng.choice(tables::BANK_PREFIXES)).to_owned();
    for _ in 0..12 {
        card.push(char::from_digit(rng.int(0, 9) as u32, 10).expect("0-9"));
    }
    card
}

/// 生成一个数字类字段（`generateNumber` 的 `switch`）。
pub fn number_one(rng: &mut Rng, kind: NumberKind) -> String {
    match kind {
        NumberKind::Phone => phone(rng),
        NumberKind::IdCard => id_card(rng),
        NumberKind::BankCard => bank_card(rng),
        NumberKind::Age => rng.int(1, 120).to_string(),
        NumberKind::AdultAge => rng.int(18, 80).to_string(),
        NumberKind::Integer => rng.int(1, 10000).to_string(),
        NumberKind::Float => format!("{:.2}", rng.float() * 9999.99),
        NumberKind::Price => format!("{:.2}", rng.float() * 9999.98 + 0.01),
        NumberKind::Percentage => format!("{:.2}%", rng.float() * 100.0),
    }
}

/// 生成 `count` 条数字类字段。
pub fn numbers(rng: &mut Rng, kind: NumberKind, count: usize) -> Vec<String> {
    (0..count).map(|_| number_one(rng, kind)).collect()
}

// ══════════════════════════════════════════════════════════════════════
// 客户信息（AML / CRS 报送口径）
// ══════════════════════════════════════════════════════════════════════

/// 账户持有人类型（参考页 `holderType`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolderType {
    Random,
    Personal,
    Institution,
}

impl HolderType {
    pub fn parse(value: &str) -> Self {
        match value {
            "personal" => Self::Personal,
            "institution" => Self::Institution,
            _ => Self::Random,
        }
    }
}

/// 与机构的关系（参考页 `relationship`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Controller,
    Shareholder,
    BeneficialOwner,
}

/// 持股比例。参考页有两种写法：股东是 `45%`，受益人是 `持股45%`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shareholding {
    pub percent: u32,
    /// `true` = 前缀「持股」（受益人），`false` = 裸百分比（股东）。
    pub with_holding_prefix: bool,
}

/// 账户状态（参考页 `accountStatus`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    /// 90% 概率。
    Active,
    /// 10% 概率。
    ClosedThisYear,
}

/// 个人（控制人 / 股东 / 受益人共用）。
#[derive(Debug, Clone)]
pub struct Person {
    pub name: String,
    pub english_name: String,
    pub id_type: String,
    pub id_number: String,
    pub nationality: String,
    pub address: String,
    pub birth_date: String,
    pub birth_place: String,
    pub tax_resident_country: String,
    pub tin: String,
    pub shareholding: Option<Shareholding>,
    pub relationship: Relation,
}

/// 机构账户的专属字段。
#[derive(Debug, Clone)]
pub struct Institution {
    pub name: String,
    pub english_name: String,
    pub address: String,
    pub registration_number: String,
    pub tin: String,
    pub is_passive_nfe: bool,
    pub is_listed: bool,
    pub stock_code: Option<String>,
}

/// 一份客户信息。
#[derive(Debug, Clone)]
pub struct Customer {
    pub is_institution: bool,
    /// 是否非居民（参考页 `isTaxResident === '是'` 表示「是税收居民」，
    /// 但它用的是 `isNonResident` 赋的值——这里按**实际含义**取名）。
    pub is_tax_resident: bool,
    pub last_name: String,
    pub first_name: String,
    pub english_name: String,
    pub chinese_name: String,
    pub id_type: String,
    pub id_number: String,
    pub nationality: String,
    pub address: String,
    pub country_code: String,
    pub tax_resident_country: String,
    pub tin: String,
    pub birth_date: String,
    pub birth_place: String,
    pub phone: String,
    pub email: String,
    pub account_number: String,
    pub account_type: String,
    pub account_open_date: String,
    pub account_balance: String,
    pub currency: String,
    pub account_status: AccountStatus,
    pub annual_interest: String,
    pub annual_dividend: String,
    pub annual_other_income: String,
    pub annual_redemption: String,
    pub institution: Option<Institution>,
    pub controller: Option<Person>,
    pub shareholders: Vec<Person>,
    pub beneficial_owners: Vec<Person>,
}

/// 客户信息选项。
#[derive(Debug, Clone)]
pub struct CustomerOptions {
    /// 国家代码（`TAX_COUNTRIES` 的键），默认 `CN`。
    pub country: String,
    pub holder_type: HolderType,
    /// 强制按非居民处理（参考页 `nonResidentOnly`）。
    pub non_resident_only: bool,
}

impl Default for CustomerOptions {
    fn default() -> Self {
        Self {
            country: "CN".to_owned(),
            holder_type: HolderType::Random,
            non_resident_only: false,
        }
    }
}

fn tax_country(code: &str) -> (&'static str, &'static str, &'static str) {
    tables::TAX_COUNTRIES
        .iter()
        .copied()
        .find(|(key, _, _)| *key == code)
        .unwrap_or(tables::FALLBACK_TAX_COUNTRY)
}

fn city_of(rng: &mut Rng, country: &str) -> String {
    if country == "CN" {
        return (*rng.choice(tables::ZH_CITIES)).to_owned();
    }
    let pool = tables::FOREIGN_CITIES
        .iter()
        .find(|(code, _)| *code == country)
        .map(|(_, cities)| *cities)
        .unwrap_or(&["Capital City"]);
    (*rng.choice(pool)).to_owned()
}

fn foreign_or_id(rng: &mut Rng, country: &str) -> String {
    if country == "CN" {
        id_card(rng)
    } else {
        format!("P{}", rng.int(100_000_000, 999_999_999))
    }
}

fn person(
    rng: &mut Rng,
    country: &str,
    tax_name: &str,
    relationship: Relation,
    shareholding: Option<Shareholding>,
) -> Person {
    let name = en_name(rng);
    Person {
        english_name: name.clone(),
        name,
        id_type: if country == "CN" {
            tables::ID_TYPE_ID_CARD.to_owned()
        } else {
            tables::ID_TYPE_PASSPORT.to_owned()
        },
        id_number: foreign_or_id(rng, country),
        nationality: (*rng.choice(tables::NATIONALITIES)).to_owned(),
        address: if country == "CN" {
            zh_address(rng, AddressOptions::default())
        } else {
            us_address(rng, false)
        },
        birth_date: datetime_one(rng, DateTimeKind::Birthdate, true),
        birth_place: format!("{tax_name} {}", city_of(rng, country)),
        tax_resident_country: tax_name.to_owned(),
        tin: rng.int(100_000_000, 999_999_999).to_string(),
        shareholding,
        relationship,
    }
}

/// 生成一份客户信息（`generateCustomer`）。
pub fn customer(rng: &mut Rng, options: &CustomerOptions) -> Customer {
    let country = options.country.as_str();
    let is_institution = match options.holder_type {
        HolderType::Institution => true,
        HolderType::Personal => false,
        HolderType::Random => rng.more_than(0.7),
    };
    let (code, tax_name, tld) = tax_country(country);
    let is_non_resident = options.non_resident_only || country != "CN";

    let english_name = en_name(rng);
    let parts: Vec<&str> = english_name.split(' ').collect();
    let first_name = parts.first().copied().unwrap_or("John").to_owned();
    let last_name = parts.get(1).copied().unwrap_or("Smith").to_owned();
    let chinese_name = if !is_non_resident || rng.more_than(0.3) {
        zh_name(rng, Gender::Any)
    } else {
        String::new()
    };

    let id_type = if country == "CN" {
        tables::ID_TYPE_ID_CARD.to_owned()
    } else if rng.more_than(0.5) {
        tables::ID_TYPE_PASSPORT.to_owned()
    } else {
        (*rng.choice(tables::ID_TYPES)).to_owned()
    };
    let id_number = foreign_or_id(rng, country);
    let nationality = (*rng.choice(tables::NATIONALITIES)).to_owned();

    let (tin, account_number) = match country {
        "CN" => (
            id_card(rng),
            format!("6225{}", rng.int(100_000_000_000, 999_999_999_999)),
        ),
        "US" => (
            format!(
                "{}-{}-{}",
                rng.int(100, 999),
                rng.int(10, 99),
                rng.int(1000, 9999)
            ),
            rng.int(100_000_000_000, 999_999_999_999).to_string(),
        ),
        _ => (
            rng.int(100_000_000, 999_999_999).to_string(),
            rng.int(100_000_000_000, 999_999_999_999).to_string(),
        ),
    };

    let open_year = rng.int(2010, 2023);
    let open_month = rng.int(1, 12);
    let open_day = rng.int(1, 28);

    let dial_code = match country {
        "CN" => "86",
        "HK" => "852",
        "SG" => "65",
        _ => "1",
    };
    // 参考页 `randomInt(13000000000, 19999999999).toString().slice(1)`：11 位去掉首位 = 10 位。
    let local = rng.int(13_000_000_000, 19_999_999_999).to_string();
    let phone = format!("+{dial_code}{}", &local[1..]);
    let email = format!(
        "{}.{}{}@{}{}",
        first_name.to_lowercase(),
        last_name.to_lowercase(),
        rng.int(1, 999),
        first_name.to_lowercase(),
        tld
    );

    let currency = if country == "CN" {
        "CNY".to_owned()
    } else {
        let pool: Vec<&str> = tables::CURRENCIES
            .iter()
            .copied()
            .filter(|code| *code != "CNY")
            .collect();
        (*rng.choice(&pool)).to_owned()
    };

    let mut customer = Customer {
        is_institution,
        is_tax_resident: !is_non_resident,
        last_name,
        first_name,
        english_name,
        chinese_name,
        id_type,
        id_number,
        nationality,
        address: if country == "CN" {
            zh_address(rng, AddressOptions::default())
        } else {
            us_address(rng, false)
        },
        country_code: code.to_owned(),
        tax_resident_country: tax_name.to_owned(),
        tin,
        // 参考页 `generateDateValue('birthdate')`：1950-2005。
        birth_date: datetime_one(rng, DateTimeKind::Birthdate, true),
        birth_place: format!("{tax_name} {}", city_of(rng, country)),
        phone,
        email,
        account_number,
        account_type: (*rng.choice(tables::ACCOUNT_TYPES)).to_owned(),
        account_open_date: format!("{open_year:04}-{open_month:02}-{open_day:02}"),
        account_balance: format!("{:.2}", rng.float() * 999_999.0 + 1000.0),
        currency,
        account_status: if rng.more_than(0.1) {
            AccountStatus::Active
        } else {
            AccountStatus::ClosedThisYear
        },
        annual_interest: format!("{:.2}", rng.float() * 50_000.0),
        annual_dividend: format!("{:.2}", rng.float() * 30_000.0),
        annual_other_income: format!("{:.2}", rng.float() * 20_000.0),
        annual_redemption: format!("{:.2}", rng.float() * 100_000.0),
        institution: None,
        controller: None,
        shareholders: Vec::new(),
        beneficial_owners: Vec::new(),
    };

    if is_institution {
        let inst_name = en_name(rng);
        let is_listed = rng.more_than(0.8);
        customer.institution = Some(Institution {
            name: format!("{inst_name}{}", tables::INSTITUTION_NAME_SUFFIX),
            english_name: format!("{inst_name}{}", tables::INSTITUTION_EN_SUFFIX),
            address: us_address(rng, false),
            registration_number: format!("91110000{}", rng.int(100_000_000, 999_999_999)),
            tin: rng.int(100_000_000, 999_999_999).to_string(),
            is_passive_nfe: rng.more_than(0.5),
            is_listed,
            stock_code: if is_listed {
                Some(format!("{}.SH", rng.int(600_000, 689_999)))
            } else {
                None
            },
        });

        if rng.more_than(0.2) {
            customer.controller = Some(person(rng, country, tax_name, Relation::Controller, None));
        }

        let shareholder_count = rng.int(1, 3);
        customer.shareholders = (0..shareholder_count)
            .map(|_| {
                let percent = rng.int(5, 50) as u32;
                person(
                    rng,
                    country,
                    tax_name,
                    Relation::Shareholder,
                    Some(Shareholding {
                        percent,
                        with_holding_prefix: false,
                    }),
                )
            })
            .collect();

        if rng.more_than(0.5) {
            let owner_count = rng.int(1, 2);
            customer.beneficial_owners = (0..owner_count)
                .map(|_| {
                    let percent = rng.int(10, 75) as u32;
                    person(
                        rng,
                        country,
                        tax_name,
                        Relation::BeneficialOwner,
                        Some(Shareholding {
                            percent,
                            with_holding_prefix: true,
                        }),
                    )
                })
                .collect();
        }
    }

    customer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng() -> Rng {
        Rng::from_seed(0x1234_5678_9ABC_DEF0)
    }

    #[test]
    fn int_stays_in_range() {
        let mut rng = rng();
        for _ in 0..500 {
            let value = rng.int(3, 7);
            assert!((3..=7).contains(&value), "{value}");
        }
    }

    #[test]
    fn int_handles_single_value_range() {
        let mut rng = rng();
        assert_eq!(rng.int(5, 5), 5);
        assert_eq!(rng.int(9, 3), 9);
    }

    #[test]
    fn float_is_in_unit_interval() {
        let mut rng = rng();
        for _ in 0..500 {
            let value = rng.float();
            assert!((0.0..1.0).contains(&value), "{value}");
        }
    }

    #[test]
    fn zh_name_is_two_or_three_characters_plus_surname() {
        let mut rng = rng();
        for _ in 0..200 {
            let name = zh_name(&mut rng, Gender::Any);
            let length = name.chars().count();
            // 复姓（欧阳/司马/上官）会让长度多 1。
            assert!((2..=4).contains(&length), "{name}");
        }
    }

    #[test]
    fn en_name_is_two_capitalized_words() {
        let mut rng = rng();
        for _ in 0..100 {
            let name = en_name(&mut rng);
            let parts: Vec<&str> = name.split(' ').collect();
            assert_eq!(parts.len(), 2, "{name}");
            for part in parts {
                assert!(
                    part.chars().next().expect("首字符").is_uppercase(),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn ja_name_has_space_between_surname_and_given() {
        let mut rng = rng();
        let name = ja_name(&mut rng);
        assert_eq!(name.matches(' ').count(), 1, "{name}");
    }

    #[test]
    fn names_with_company_use_newline_for_single_and_pipe_for_batch() {
        let mut rng = rng();
        let options = NameOptions {
            with_company: true,
            ..NameOptions::default()
        };
        let single = names(&mut rng, options, 1);
        assert!(single[0].contains('\n'), "{}", single[0]);
        let batch = names(&mut rng, options, 3);
        assert!(batch.iter().all(|name| name.contains(" | ")));
    }

    #[test]
    fn zh_address_contains_number_sign() {
        let mut rng = rng();
        let address = zh_address(&mut rng, AddressOptions::default());
        // 默认带邮编，所以「号」不在末尾；门牌号后缀本身就是数据表里的 `ZH_NUMBER_SUFFIX`。
        assert!(address.contains(tables::ZH_NUMBER_SUFFIX), "{address}");
        assert!(
            tables::ZH_PROVINCES
                .iter()
                .any(|province| address.starts_with(province)),
            "{address}"
        );
    }

    #[test]
    fn zh_address_with_postcode_matches_reference_shape() {
        let mut rng = rng();
        let address = zh_address(
            &mut rng,
            AddressOptions {
                with_postal_code: true,
                ..AddressOptions::default()
            },
        );
        assert!(address.contains(" 邮编"), "{address}");
    }

    #[test]
    fn us_address_always_has_zip() {
        let mut rng = rng();
        let address = us_address(&mut rng, false);
        let tail = address.rsplit(' ').next().expect("邮编");
        assert_eq!(tail.len(), 5, "{address}");
    }

    #[test]
    fn coordinate_suffix_matches_reference_format() {
        let mut rng = rng();
        let address = us_address(&mut rng, true);
        assert!(address.contains(" ("), "{address}");
        assert!(address.ends_with(')'), "{address}");
    }

    #[test]
    fn internet_shapes() {
        let mut rng = rng();
        let mail = email(&mut rng);
        assert!(mail.contains('@'), "{mail}");

        let resolved = ipv4(&mut rng);
        assert_eq!(resolved.split('.').count(), 4, "{resolved}");

        let v6 = ipv6(&mut rng);
        assert_eq!(v6.split(':').count(), 8, "{v6}");

        let hardware = mac(&mut rng);
        assert_eq!(hardware.split(':').count(), 6, "{hardware}");
        assert_eq!(hardware.len(), 17, "{hardware}");

        let id = uuid(&mut rng);
        assert_eq!(id.len(), 36, "{id}");
        assert_eq!(id.matches('-').count(), 4, "{id}");

        let site = domain(&mut rng);
        assert!(site.contains('.'), "{site}");
    }

    #[test]
    fn internet_url_uses_http_or_https() {
        let mut rng = rng();
        for _ in 0..50 {
            let value = url(&mut rng);
            assert!(
                value.starts_with("http://") || value.starts_with("https://"),
                "{value}"
            );
        }
    }

    #[test]
    fn password_is_within_reference_length() {
        let mut rng = rng();
        for _ in 0..50 {
            let value = password(&mut rng);
            assert!((12..=20).contains(&value.len()), "{value}");
        }
    }

    #[test]
    fn username_allows_underscore() {
        let mut rng = rng();
        for _ in 0..50 {
            let value = username(&mut rng);
            assert!((6..=16).contains(&value.len()), "{value}");
            assert!(
                value
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{value}"
            );
        }
    }

    #[test]
    fn date_shape_is_iso() {
        let mut rng = rng();
        let value = datetime_one(&mut rng, DateTimeKind::Date, true);
        assert_eq!(value.len(), 10, "{value}");
        assert_eq!(value.matches('-').count(), 2, "{value}");
    }

    #[test]
    fn single_datetime_is_today() {
        let mut rng = rng();
        let today = Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(datetime_one(&mut rng, DateTimeKind::Date, false), today);
    }

    #[test]
    fn batch_time_is_zero_padded() {
        let mut rng = rng();
        let value = datetime_one(&mut rng, DateTimeKind::Time, true);
        assert_eq!(value.len(), 8, "{value}");
        assert_eq!(value.matches(':').count(), 2, "{value}");
    }

    #[test]
    fn weekday_comes_from_reference_table() {
        let mut rng = rng();
        let value = datetime_one(&mut rng, DateTimeKind::Weekday, true);
        assert!(tables::WEEKDAYS.contains(&value.as_str()), "{value}");
    }

    #[test]
    fn birthday_is_adult_and_within_80() {
        let mut rng = rng();
        let this_year = Local::now().year();
        for _ in 0..50 {
            let value = datetime_one(&mut rng, DateTimeKind::Birthday, false);
            let year: i32 = value[..4].parse().expect("年份");
            let age = this_year - year;
            assert!((18..=80).contains(&age), "{value} -> {age}");
        }
    }

    #[test]
    fn birthdate_is_between_1950_and_2005() {
        let mut rng = rng();
        for _ in 0..50 {
            let value = datetime_one(&mut rng, DateTimeKind::Birthdate, true);
            let year: i32 = value[..4].parse().expect("年份");
            assert!((1950..=2005).contains(&year), "{value}");
        }
    }

    #[test]
    fn id_card_check_digit_matches_gb11643() {
        // 一个公开的样例号（前 17 位）与其校验位。
        assert_eq!(id_check_digit("11010519491231002"), 'X');
        assert_eq!(id_check_digit("44052418800101001"), '4');
    }

    #[test]
    fn id_card_is_18_chars_with_valid_check_digit() {
        let mut rng = rng();
        for _ in 0..50 {
            let card = id_card(&mut rng);
            assert_eq!(card.len(), 18, "{card}");
            assert_eq!(
                id_check_digit(&card[..17]),
                card.chars().nth(17).expect("校验位")
            );
        }
    }

    #[test]
    fn phone_is_eleven_digits_with_known_prefix() {
        let mut rng = rng();
        for _ in 0..50 {
            let value = phone(&mut rng);
            assert_eq!(value.len(), 11, "{value}");
            assert!(tables::PHONE_PREFIXES.contains(&&value[..3]), "{value}");
        }
    }

    #[test]
    fn bank_card_is_sixteen_digits() {
        let mut rng = rng();
        let value = bank_card(&mut rng);
        assert_eq!(value.len(), 16, "{value}");
        assert!(value.chars().all(|c| c.is_ascii_digit()), "{value}");
    }

    #[test]
    fn percentage_has_percent_suffix() {
        let mut rng = rng();
        let value = number_one(&mut rng, NumberKind::Percentage);
        assert!(value.ends_with('%'), "{value}");
    }

    #[test]
    fn price_and_float_have_two_decimals() {
        let mut rng = rng();
        for kind in [NumberKind::Float, NumberKind::Price] {
            let value = number_one(&mut rng, kind);
            let decimals = value.split('.').nth(1).expect("小数位");
            assert_eq!(decimals.len(), 2, "{value}");
        }
    }

    #[test]
    fn personal_customer_has_no_institution_block() {
        let mut rng = rng();
        let options = CustomerOptions {
            holder_type: HolderType::Personal,
            ..CustomerOptions::default()
        };
        let info = customer(&mut rng, &options);
        assert!(!info.is_institution);
        assert!(info.institution.is_none());
        assert!(info.controller.is_none());
        assert!(info.shareholders.is_empty());
    }

    #[test]
    fn institution_customer_fills_tree() {
        let mut rng = rng();
        let options = CustomerOptions {
            holder_type: HolderType::Institution,
            ..CustomerOptions::default()
        };
        let info = customer(&mut rng, &options);
        assert!(info.is_institution);
        let institution = info.institution.as_ref().expect("机构块");
        assert!(institution.name.contains("投资股份有限公司"));
        assert!((1..=3).contains(&info.shareholders.len()));
        if institution.is_listed {
            assert!(
                institution
                    .stock_code
                    .as_deref()
                    .unwrap_or("")
                    .ends_with(".SH")
            );
        } else {
            assert!(institution.stock_code.is_none());
        }
    }

    #[test]
    fn non_resident_flag_follows_country() {
        let mut rng = rng();
        let cn = customer(&mut rng, &CustomerOptions::default());
        assert!(cn.is_tax_resident, "CN 应为税收居民");

        let us = customer(
            &mut rng,
            &CustomerOptions {
                country: "US".to_owned(),
                ..CustomerOptions::default()
            },
        );
        assert!(!us.is_tax_resident, "非 CN 应为非居民");
        assert_eq!(us.country_code, "US");
        assert_eq!(us.tax_resident_country, "美国");
    }

    #[test]
    fn unknown_country_falls_back_to_cn_tax_table() {
        let mut rng = rng();
        let info = customer(
            &mut rng,
            &CustomerOptions {
                country: "ZZ".to_owned(),
                ..CustomerOptions::default()
            },
        );
        assert_eq!(info.country_code, "CN");
    }

    #[test]
    fn shareholder_and_owner_shareholding_shapes_differ() {
        let mut rng = rng();
        let info = customer(
            &mut rng,
            &CustomerOptions {
                holder_type: HolderType::Institution,
                ..CustomerOptions::default()
            },
        );
        for holder in &info.shareholders {
            let share = holder.shareholding.expect("股东有持股比例");
            assert!(!share.with_holding_prefix);
            assert!((5..=50).contains(&share.percent));
        }
        for owner in &info.beneficial_owners {
            let share = owner.shareholding.expect("受益人有持股比例");
            assert!(share.with_holding_prefix);
            assert!((10..=75).contains(&share.percent));
        }
    }

    #[test]
    fn customer_phone_uses_country_dial_code() {
        let mut rng = rng();
        let cn = customer(&mut rng, &CustomerOptions::default());
        assert!(cn.phone.starts_with("+86"), "{}", cn.phone);
        let hk = customer(
            &mut rng,
            &CustomerOptions {
                country: "HK".to_owned(),
                ..CustomerOptions::default()
            },
        );
        assert!(hk.phone.starts_with("+852"), "{}", hk.phone);
    }

    #[test]
    fn customer_account_number_starts_with_union_pay_bin() {
        let mut rng = rng();
        let info = customer(&mut rng, &CustomerOptions::default());
        assert!(
            info.account_number.starts_with("6225"),
            "{}",
            info.account_number
        );
    }

    #[test]
    fn seeds_are_reproducible() {
        let first = {
            let mut rng = Rng::from_seed(7);
            names(&mut rng, NameOptions::default(), 5)
        };
        let second = {
            let mut rng = Rng::from_seed(7);
            names(&mut rng, NameOptions::default(), 5)
        };
        assert_eq!(first, second);
    }

    #[test]
    fn kinds_parse_from_reference_keys() {
        assert_eq!(Lang::parse("en"), Lang::En);
        assert_eq!(Lang::parse("ja"), Lang::Ja);
        assert_eq!(Lang::parse("zh"), Lang::Zh);
        assert_eq!(Gender::parse("male"), Gender::Male);
        assert_eq!(Gender::parse(""), Gender::Any);
        assert_eq!(InternetKind::parse("uuid"), InternetKind::Uuid);
        assert_eq!(DateTimeKind::parse("birthday"), DateTimeKind::Birthday);
        assert_eq!(NumberKind::parse("adult_age"), NumberKind::AdultAge);
        assert_eq!(HolderType::parse("institution"), HolderType::Institution);
    }
}
