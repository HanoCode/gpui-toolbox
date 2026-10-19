//! 随机生成器的**素材词表**（参考仓 `RandomTextGenerator.vue` 顶部那一大片常量）。
//!
//! 这里的中文**不是界面文案**，而是生成结果本身：生成的姓名就叫「王伟」，地址就叫
//! 「北京市春兰市长安路 12 号」，账户类型就叫「存款账户」。把它们翻译成英文，
//! 生成出来的「中文姓名 / 中国地址 / 中文业务术语」测试数据就跑不通被测系统了。
//!
//! 所以本文件在 `duck-theme` 的 `NON_UI_SOURCES` 里豁免（判据是「会不会被渲染成
//! 界面文字」，不是「是不是中文」）；界面上那些**标签**（「账户持有人类型」「制表符」）
//! 全部在 `plugins/text-random-gen/locales/*.toml` 里登记，一个都没漏。
//!
//! 逐条对应关系见每个常量上面的注释（括号里是参考仓的常量名）。

/// 中文姓氏（`ZH_SURNAMES`），含复姓。
pub const ZH_SURNAMES: &[&str] = &[
    "王", "李", "张", "刘", "陈", "杨", "赵", "黄", "周", "吴", "徐", "孙", "胡", "朱", "高", "林",
    "何", "郭", "马", "罗", "梁", "宋", "郑", "谢", "韩", "唐", "冯", "于", "董", "萧", "程", "曹",
    "袁", "邓", "许", "傅", "沈", "曾", "彭", "吕", "苏", "卢", "蒋", "蔡", "贾", "丁", "魏", "薛",
    "叶", "阎", "欧阳", "司马", "上官",
];

/// 男性名字用字（`ZH_GIVEN_MALE`）。
pub const ZH_GIVEN_MALE: &[&str] = &[
    "伟", "强", "明", "华", "建", "文", "志", "勇", "军", "杰", "涛", "超", "辉", "鹏", "磊", "峰",
    "龙", "虎", "豪", "俊", "翔", "宇", "浩", "凯", "睿", "毅", "俊", "博", "晨", "旭", "阳",
];

/// 女性名字用字（`ZH_GIVEN_FEMALE`）。
pub const ZH_GIVEN_FEMALE: &[&str] = &[
    "芳", "娜", "敏", "静", "丽", "艳", "娟", "秀", "美", "玲", "婷", "雯", "慧", "琳", "颖", "洁",
    "莉", "萍", "红", "燕", "霞", "云", "兰", "英", "梅", "莲", "桂", "珍", "贞", "莉", "莎",
];

/// 中性名字用字（`ZH_GIVEN_NEUTRAL`）。
pub const ZH_GIVEN_NEUTRAL: &[&str] = &[
    "德", "仁", "义", "礼", "智", "信", "忠", "孝", "善", "美", "真", "诚", "正", "雅", "和", "平",
    "刚", "柔", "欣", "怡",
];

/// 英文名前半（`EN_FIRST_PREFIX`）。
pub const EN_FIRST_PREFIX: &[&str] = &[
    "Jam", "Rob", "Joh", "Mich", "Dav", "Will", "Rich", "Thom", "Chr", "Char", "Dan", "Matt",
    "Ant", "Mar", "Don", "Stev", "Pau", "And", "Josh", "Ken", "Kev", "Br", "Geo", "Tim", "Ron",
    "Edw", "Jeff", "Ry", "Jac", "Gar", "Amy", "Bel", "Car", "Dor", "Eve", "Fay", "Gre", "Ivy",
    "Jan", "Kar", "Lyn", "May", "Nan", "Pat", "Ray", "Sab", "Val",
];

/// 英文名后半（`EN_FIRST_SUFFIX`，原表里 `es` 出现两次，照抄）。
pub const EN_FIRST_SUFFIX: &[&str] = &[
    "es", "ert", "n", "ael", "id", "iam", "ard", "es", "istopher", "les", "iel", "hew", "hony",
    "k", "ald", "en", "l", "rew", "ua", "neth", "in", "ian", "rge", "othy", "on", "ard", "rey",
    "an", "ob", "y", "a", "ie", "ly", "na", "ra", "sy", "na", "ri", "li", "nn", "ri", "le",
];

/// 英文姓前半（`EN_LAST_PREFIX`）。
pub const EN_LAST_PREFIX: &[&str] = &[
    "Sm", "John", "Will", "Br", "Jon", "Garc", "Mill", "Dav", "Rodr", "Mart", "Hern", "Lop",
    "Gonz", "Wils", "And", "Thom", "Tayl", "Moor", "Jack", "Per", "Thomp", "Wh", "Harr", "Sanch",
    "Cl", "Ram", "Lew", "Rob", "Ad", "Ba", "Ca", "Co", "De", "Fa", "Ga",
];

/// 英文姓后半（`EN_LAST_SUFFIX`）。
pub const EN_LAST_SUFFIX: &[&str] = &[
    "ith", "son", "iams", "own", "es", "ia", "er", "is", "iguez", "inez", "andez", "ez", "alez",
    "on", "erson", "as", "or", "e", "son", "in", "ee", "ez", "son", "ite", "is", "ark", "irez",
    "is", "inson", "er", "ton", "ling", "well", "man", "ford", "berry", "ston", "wort", "ford",
];

/// 日文姓（`JA_SURNAME`，原表 `鈴木` / `佐藤` 重复，照抄）。
pub const JA_SURNAME: &[&str] = &[
    "田中",
    "山本",
    "高橋",
    "渡辺",
    "伊藤",
    "鈴木",
    "佐藤",
    "佐々木",
    "斎藤",
    "松本",
    "井上",
    "小林",
    "木村",
    "加藤",
    "吉田",
    "山田",
    "中村",
    "山口",
    "小川",
    "堀",
    "鈴木",
    "佐藤",
];

/// 日文名起首（`JA_GIVEN_FIRST`）。
pub const JA_GIVEN_FIRST: &[&str] = &[
    "あ", "か", "さ", "た", "な", "は", "ま", "や", "ら", "わ", "こ", "み", "き", "ゆ", "り",
    "あさ", "かお", "みほ",
];

/// 日文名结尾（`JA_GIVEN_ENDING`）。
pub const JA_GIVEN_ENDING: &[&str] = &[
    "子", "美", "恵", "香", "代", "花", "愛", "里", "奈", "菜", "穂", "華", "雪", "彩", "環", "樹",
    "海", "風", "光",
];

/// 公司名前缀（`generateCompany` 的 `prefixes`）。
pub const COMPANY_PREFIX: &[&str] = &[
    "北京", "上海", "深圳", "广州", "杭州", "成都", "武汉", "西安", "南京", "苏州",
];

/// 公司名行业（`generateCompany` 的 `types`）。
pub const COMPANY_TYPE: &[&str] = &[
    "科技", "信息", "网络", "软件", "数据", "智能", "创新", "发展", "贸易", "实业",
];

/// 公司名后缀（`generateCompany` 的 `suffixes`）。
pub const COMPANY_SUFFIX: &[&str] = &[
    "有限公司",
    "股份有限公司",
    "科技有限公司",
    "集团有限公司",
    "实业有限公司",
];

/// 省级行政区（`ZH_PROVINCES`）。
pub const ZH_PROVINCES: &[&str] = &[
    "北京市",
    "天津市",
    "上海市",
    "重庆市",
    "河北省",
    "山西省",
    "辽宁省",
    "吉林省",
    "黑龙江省",
    "江苏省",
    "浙江省",
    "安徽省",
    "福建省",
    "江西省",
    "山东省",
    "河南省",
    "湖北省",
    "湖南省",
    "广东省",
    "海南省",
    "四川省",
    "贵州省",
    "云南省",
    "陕西省",
    "甘肃省",
    "青海省",
    "内蒙古自治区",
    "广西壮族自治区",
    "西藏自治区",
    "宁夏回族自治区",
    "新疆维吾尔自治区",
];

/// 街道名前缀（`ZH_STREET_PREFIX`）。
pub const ZH_STREET_PREFIX: &[&str] = &[
    "长安", "中山", "人民", "解放", "建设", "和平", "新华", "胜利", "红旗", "工农", "东风", "幸福",
    "团结", "民主", "复兴",
];

/// 街道名后缀（`ZH_STREET_SUFFIX`）。
pub const ZH_STREET_SUFFIX: &[&str] = &["街", "路", "大道", "巷", "胡同", "里"];

/// 中文名「市/县/区」的随机用字池（`generateZhAddress` 里的 `hanzi`）。
///
/// 原串 `春夏秋冬梅兰竹菊花木水火土金木天地人和` 里「木」出现两次，照抄不去重。
pub const ZH_CITY_HANZI: &[char] = &[
    '春', '夏', '秋', '冬', '梅', '兰', '竹', '菊', '花', '木', '水', '火', '土', '金', '木', '天',
    '地', '人', '和',
];

/// 中文名后缀（`generateZhAddress` 的 `['市','县','区']`）。
pub const ZH_CITY_SUFFIX: &[&str] = &["市", "县", "区"];

/// 美国州名（`US_STATES`）。
pub const US_STATES: &[&str] = &[
    "California",
    "Texas",
    "Florida",
    "New York",
    "Illinois",
    "Pennsylvania",
    "Ohio",
    "Georgia",
    "North Carolina",
    "Michigan",
];

/// 美国城市（`US_CITIES`）。
pub const US_CITIES: &[&str] = &[
    "Los Angeles",
    "Houston",
    "Phoenix",
    "Philadelphia",
    "San Antonio",
    "San Diego",
    "Dallas",
    "San Jose",
    "Austin",
    "Jacksonville",
];

/// 美国街道（`generateUsAddress` 的 `streets`）。
pub const US_STREETS: &[&str] = &[
    "Main St",
    "Oak Ave",
    "Pine Rd",
    "Elm Dr",
    "Maple Ln",
    "Cedar Blvd",
    "Birch Way",
    "Walnut Ct",
];

/// 邮箱域名（`EMAIL_DOMAINS`）。
pub const EMAIL_DOMAINS: &[&str] = &[
    "gmail.com",
    "yahoo.com",
    "hotmail.com",
    "outlook.com",
    "qq.com",
    "163.com",
    "126.com",
    "sina.com",
    "sohu.com",
];

/// 顶级域（`TLDS`）。
pub const TLDS: &[&str] = &["com", "org", "net", "edu", "gov", "cn", "uk", "de", "jp"];

/// 手机号段（`PHONE_PREFIXES`）。
pub const PHONE_PREFIXES: &[&str] = &[
    "130", "131", "132", "133", "134", "135", "136", "137", "138", "139", "150", "151", "152",
    "158", "159", "170", "171", "172", "173", "176", "177", "178", "179", "180", "181", "182",
    "183", "184", "185", "186", "187", "188", "189",
];

/// 身份证前 6 位行政区划码（`AREA_CODES`）。
pub const AREA_CODES: &[&str] = &[
    "110000", "120000", "130000", "140000", "150000", "210000", "220000", "230000", "310000",
    "320000", "330000", "340000", "350000", "360000", "370000", "410000", "420000", "430000",
    "440000", "450000", "500000", "510000", "520000", "530000", "610000", "620000", "630000",
    "640000", "650000",
];

/// 银行卡 BIN（`BANK_PREFIXES`）。
pub const BANK_PREFIXES: &[&str] = &[
    "6225", "6227", "6228", "6229", "6230", "6210", "6231", "6216",
];

/// 账户类型（`ACCOUNT_TYPES`）——生成结果里的业务术语，不翻译。
pub const ACCOUNT_TYPES: &[&str] = &["存款账户", "托管账户", "具有现金价值的保险合同", "其他账户"];

/// 币种（`CURRENCIES`）。
pub const CURRENCIES: &[&str] = &["USD", "EUR", "GBP", "CNY", "JPY", "HKD", "SGD"];

/// 证件类型（`ID_TYPES`）。
pub const ID_TYPES: &[&str] = &[
    "身份证",
    "护照",
    "港澳通行证",
    "台湾居民来往大陆通行证",
    "外国人居留证",
];

/// 国籍（`NATIONALITIES`）。
pub const NATIONALITIES: &[&str] = &[
    "中国",
    "美国",
    "英国",
    "法国",
    "德国",
    "日本",
    "韩国",
    "新加坡",
    "澳大利亚",
    "加拿大",
    "香港",
    "澳门",
    "台湾",
];

/// 星期名（`generateDateValue` 的 `days`）。
pub const WEEKDAYS: &[&str] = &[
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// 税收居民国表（`TAX_COUNTRIES`）：`(代码, 中文名, 邮箱顶级域)`。
pub const TAX_COUNTRIES: &[(&str, &str, &str)] = &[
    ("CN", "中国", ".cn"),
    ("US", "美国", ".com"),
    ("HK", "香港", ".com.hk"),
    ("SG", "新加坡", ".com.sg"),
    ("JP", "日本", ".co.jp"),
    ("CA", "加拿大", ".ca"),
    ("GB", "英国", ".co.uk"),
    ("DE", "德国", ".de"),
    ("AU", "澳大利亚", ".com.au"),
    ("FR", "法国", ".fr"),
    ("MC", "摩纳哥", ".mc"),
    ("CH", "瑞士", ".ch"),
    ("LU", "卢森堡", ".lu"),
    ("NL", "荷兰", ".nl"),
    ("SE", "瑞典", ".se"),
    ("NO", "挪威", ".no"),
    ("DK", "丹麦", ".dk"),
    ("FI", "芬兰", ".fi"),
    ("IT", "意大利", ".it"),
    ("ES", "西班牙", ".es"),
    ("PT", "葡萄牙", ".pt"),
    ("BE", "比利时", ".be"),
    ("AT", "奥地利", ".at"),
    ("IE", "爱尔兰", ".ie"),
    ("NZ", "新西兰", ".co.nz"),
];

/// 中国城市（`generateCustomer` 的 `zhCities`）。
pub const ZH_CITIES: &[&str] = &[
    "北京", "上海", "广州", "深圳", "杭州", "成都", "武汉", "西安", "南京", "苏州", "天津", "重庆",
    "青岛", "厦门", "长沙",
];

/// 海外城市（`generateCustomer` 的 `foreignCities`）：国家代码 → 城市表。
pub const FOREIGN_CITIES: &[(&str, &[&str])] = &[
    (
        "US",
        &[
            "New York",
            "Los Angeles",
            "Chicago",
            "Houston",
            "San Francisco",
            "Boston",
            "Seattle",
        ],
    ),
    ("HK", &["Hong Kong", "Kowloon", "New Territories"]),
    ("SG", &["Singapore", "Central", "Orchard"]),
    ("JP", &["Tokyo", "Osaka", "Kyoto", "Yokohama"]),
    ("GB", &["London", "Manchester", "Birmingham", "Edinburgh"]),
    ("CA", &["Toronto", "Vancouver", "Montreal", "Calgary"]),
    ("AU", &["Sydney", "Melbourne", "Brisbane", "Perth"]),
    ("DE", &["Berlin", "Munich", "Frankfurt", "Hamburg"]),
    ("FR", &["Paris", "Lyon", "Marseille", "Toulouse"]),
];

/// 国家代码在税表里的兜底（参考仓 `TAX_COUNTRIES[country] || TAX_COUNTRIES['CN']`）。
pub const FALLBACK_TAX_COUNTRY: (&str, &str, &str) = ("CN", "中国", ".cn");

/// 门牌号后缀（`generateZhAddress` 的 `` `${number}号` ``）。
pub const ZH_NUMBER_SUFFIX: &str = "号";

/// 邮编前缀（`generateZhAddress` 的 `` ` 邮编${...}` ``，**含前导空格**）。
pub const ZH_POSTCODE_PREFIX: &str = " 邮编";

/// 机构名前缀的国家后缀（`generateCustomer` 的 `` `${name}投资股份有限公司` ``）。
pub const INSTITUTION_NAME_SUFFIX: &str = "投资股份有限公司";

/// 机构名英文后缀（`generateCustomer` 的 `` `${name} Holdings Ltd.` ``）。
pub const INSTITUTION_EN_SUFFIX: &str = " Holdings Ltd.";

/// 身份证（`ID_TYPES[0]`）：中文证件类型是数据，走同一张表。
pub const ID_TYPE_ID_CARD: &str = "身份证";

/// 护照（`ID_TYPES[1]`）。
pub const ID_TYPE_PASSPORT: &str = "护照";
