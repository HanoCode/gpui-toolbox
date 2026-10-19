//! 客户信息报告的**文本排版**（参考仓 `copyCustomerInfo` 的那份串）。
//!
//! 为什么单独一个文件：内核（`duck_files_core::datagen`）只产出结构化的
//! [`Customer`]，**一个中文都不带**；报告的字段标签是界面文案，必须走译文表，
//! 所以放在工具层，逐条 `text.get(...)`。
//!
//! 版面与参考页逐行对齐（段落顺序、`--- 股东1 ---` 分隔线、空行位置都一样），
//! 因为用户是拿这份文本去填 AML / CRS 报送模板的，行的顺序变了就要重新核对。
//!
//! 三处「参考页取名有坑，这里按实际含义写」：
//!
//! 1. 参考页把 `isTaxResident` 赋成 `isNonResident`，再在复制时翻一次
//!    （`isTaxResident === '是' ? '非居民' : '居民'`）。我们直接用 `is_tax_resident`
//!    的布尔含义，只翻一次。
//! 2. 股东的持股比例是裸 `45%`，受益人是 `持股45%`——同一个字段两种写法，照抄。
//! 3. 机构块只有在 `isInstitution` 为真时才出现，控制人 / 股东 / 受益人是三段各自
//!    独立的可选块。

use duck_files_core::{AccountStatus, Customer, Person, Relation, Shareholding};

/// 取词闭包：传入中文源串，返回当前语言的说法。
pub struct Labels<'a> {
    pub get: &'a dyn Fn(&str) -> String,
}

impl Labels<'_> {
    fn t(&self, source: &str) -> String {
        (self.get)(source)
    }
}

/// 渲染整份报告。
pub fn render(customer: &Customer, labels: &Labels<'_>) -> String {
    let mut lines: Vec<String> = Vec::new();

    lines.push(format!("【{}】", labels.t("一、核心身份标识")));
    lines.push(format!(
        "{}: {}",
        labels.t("账户持有人类型"),
        if customer.is_institution {
            labels.t("机构")
        } else {
            labels.t("个人")
        }
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("税收居民身份"),
        if customer.is_tax_resident {
            labels.t("居民")
        } else {
            labels.t("非居民")
        }
    ));
    lines.push(format!("{}: {}", labels.t("国籍"), customer.nationality));
    lines.push(format!(
        "{}: {}",
        labels.t("英文姓(Surname)"),
        customer.last_name
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("英文名(Given Name)"),
        customer.first_name
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("英文全名"),
        customer.english_name
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("中文姓名"),
        if customer.chinese_name.is_empty() {
            "-".to_owned()
        } else {
            customer.chinese_name.clone()
        }
    ));
    lines.push(format!("{}: {}", labels.t("证件类型"), customer.id_type));
    lines.push(format!("{}: {}", labels.t("证件号码"), customer.id_number));
    lines.push(format!("{}: {}", labels.t("现居住地址"), customer.address));
    lines.push(format!("{}: {}", labels.t("联系电话"), customer.phone));
    lines.push(format!("{}: {}", labels.t("电子邮箱"), customer.email));
    lines.push(format!(
        "{}: {}",
        labels.t("国家/地区代码"),
        customer.country_code
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("税收居民国（地区）"),
        customer.tax_resident_country
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("纳税人识别号(TIN)"),
        customer.tin
    ));
    lines.push(format!("{}: {}", labels.t("出生日期"), customer.birth_date));
    lines.push(format!("{}: {}", labels.t("出生地"), customer.birth_place));

    lines.push(String::new());
    lines.push(format!("【{}】", labels.t("二、账户信息")));
    lines.push(format!(
        "{}: {}",
        labels.t("账户号码/账号"),
        customer.account_number
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("账户类型"),
        customer.account_type
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("账户开立日期"),
        customer.account_open_date
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("账户余额/净值（年末）"),
        customer.account_balance
    ));
    lines.push(format!("{}: {}", labels.t("币种"), customer.currency));
    lines.push(format!(
        "{}: {}",
        labels.t("账户状态"),
        match customer.account_status {
            AccountStatus::Active => labels.t("正常"),
            AccountStatus::ClosedThisYear => labels.t("本年注销"),
        }
    ));

    lines.push(String::new());
    lines.push(format!("【{}】", labels.t("三、财务信息")));
    lines.push(format!(
        "{}: {}",
        labels.t("年度利息总额"),
        customer.annual_interest
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("年度股息总额"),
        customer.annual_dividend
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("年度其他收入总额"),
        customer.annual_other_income
    ));
    lines.push(format!(
        "{}: {}",
        labels.t("销售/赎回金融资产收入总额"),
        customer.annual_redemption
    ));

    if let Some(institution) = &customer.institution {
        lines.push(String::new());
        lines.push(format!("【{}】", labels.t("四、机构账户特有字段")));
        lines.push(format!(
            "{}: {}",
            labels.t("机构名称（中文）"),
            institution.name
        ));
        lines.push(format!(
            "{}: {}",
            labels.t("机构名称（英文）"),
            institution.english_name
        ));
        lines.push(format!(
            "{}: {}",
            labels.t("机构注册地址"),
            institution.address
        ));
        lines.push(format!(
            "{}: {}",
            labels.t("机构注册号码"),
            institution.registration_number
        ));
        lines.push(format!("{}: {}", labels.t("机构TIN"), institution.tin));
        lines.push(format!(
            "{}: {}",
            labels.t("是否为消极非金融机构"),
            yes_no(institution.is_passive_nfe, labels)
        ));
        lines.push(format!(
            "{}: {}",
            labels.t("是否上市公司"),
            yes_no(institution.is_listed, labels)
        ));
        if let Some(code) = &institution.stock_code {
            lines.push(format!("{}: {code}", labels.t("股票代码")));
        }

        if let Some(controller) = &customer.controller {
            lines.push(String::new());
            lines.push(format!("【{}】", labels.t("控制人信息")));
            lines.push(format!("{}: {}", labels.t("控制人姓名"), controller.name));
            push_person_common(&mut lines, controller, labels, "控制人");
            lines.push(format!(
                "{}: {}",
                labels.t("控制人地址"),
                controller.address
            ));
            lines.push(format!(
                "{}: {}",
                labels.t("出生日期"),
                controller.birth_date
            ));
            lines.push(format!(
                "{}: {}",
                labels.t("出生地"),
                controller.birth_place
            ));
            lines.push(format!(
                "{}: {}",
                labels.t("与机构关系"),
                relation_label(controller.relationship, labels)
            ));
        }

        if !customer.shareholders.is_empty() {
            lines.push(String::new());
            lines.push(format!("【{}】", labels.t("股东信息")));
            for (index, holder) in customer.shareholders.iter().enumerate() {
                lines.push(format!(
                    "--- {} ---",
                    fill(&labels.t("股东{}"), &[(index + 1).to_string()])
                ));
                push_holder(&mut lines, holder, labels, "股东");
            }
        }

        if !customer.beneficial_owners.is_empty() {
            lines.push(String::new());
            lines.push(format!("【{}】", labels.t("受益人信息")));
            for (index, owner) in customer.beneficial_owners.iter().enumerate() {
                lines.push(format!(
                    "--- {} ---",
                    fill(&labels.t("受益人{}"), &[(index + 1).to_string()])
                ));
                push_holder(&mut lines, owner, labels, "受益人");
            }
        }
    }

    lines.join("\n")
}

fn push_holder(lines: &mut Vec<String>, holder: &Person, labels: &Labels<'_>, role: &str) {
    lines.push(format!(
        "{}: {}",
        fill(&labels.t("{}姓名"), &[labels.t(role)]),
        holder.name
    ));
    push_person_common(lines, holder, labels, role);
    if let Some(share) = holder.shareholding {
        lines.push(format!(
            "{}: {}",
            labels.t("持股比例"),
            shareholding_text(share, labels)
        ));
    }
    lines.push(format!("{}: {}", labels.t("股东地址"), holder.address));
    lines.push(format!("{}: {}", labels.t("出生日期"), holder.birth_date));
    lines.push(format!("{}: {}", labels.t("出生地"), holder.birth_place));
    if holder.relationship == Relation::BeneficialOwner {
        lines.push(format!(
            "{}: {}",
            labels.t("与机构关系"),
            relation_label(holder.relationship, labels)
        ));
    }
}

/// 证件类型 / 证件号码 / 国籍 / TIN 这四行三段共用（参考页也是复制的四行）。
fn push_person_common(lines: &mut Vec<String>, person: &Person, labels: &Labels<'_>, role: &str) {
    lines.push(format!("{}: {}", labels.t("证件类型"), person.id_type));
    lines.push(format!("{}: {}", labels.t("证件号码"), person.id_number));
    lines.push(format!("{}: {}", labels.t("国籍"), person.nationality));
    lines.push(format!(
        "{}: {}",
        labels.t("税收居民国"),
        person.tax_resident_country
    ));
    // 「控制人TIN / 股东TIN / 受益人TIN」：英文词序不同，所以走 `{}TIN` 模板。
    lines.push(format!(
        "{}: {}",
        fill(&labels.t("{}TIN"), &[labels.t(role)]),
        person.tin
    ));
}

fn yes_no(value: bool, labels: &Labels<'_>) -> String {
    if value {
        labels.t("是")
    } else {
        labels.t("否")
    }
}

fn relation_label(relation: Relation, labels: &Labels<'_>) -> String {
    match relation {
        Relation::Controller => labels.t("实际控制人"),
        Relation::Shareholder => labels.t("股东"),
        Relation::BeneficialOwner => labels.t("受益人"),
    }
}

fn shareholding_text(share: Shareholding, labels: &Labels<'_>) -> String {
    if share.with_holding_prefix {
        fill(&labels.t("持股{}%"), &[share.percent.to_string()])
    } else {
        // 裸百分比不含中文，不需要过译文表。
        format!("{}%", share.percent)
    }
}

/// 用 `{}` 占位符回填（`i18n::fill` 的 `&[String]` 薄封装）。
fn fill(template: &str, values: &[String]) -> String {
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    crate::i18n::fill(template, &refs)
}
