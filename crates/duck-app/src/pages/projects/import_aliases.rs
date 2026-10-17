//! 导入用的**输入侧别名表**：把用户文件里的列名 / 取值写法翻成内核枚举。
//!
//! # 为什么这个文件里的中文不登记进 EN 表
//!
//! 这里的每一个中文字面量都是拿去和**用户文件里的内容**做比对的输入侧字面量
//! （`"任务标题"`、`"已完成"`、`"重要不紧急"` …），一个字节都不会渲染到屏幕上 ——
//! 所以进了 `duck-theme/src/i18n.rs` 的 `NON_UI_SOURCES` 名单，与
//! `duck-mail` 的 IMAP 文件夹别名表同一个判据（「会不会被渲染」，不是「是不是中文」）。
//!
//! ⚠️ **界面文本一个字都不许放进来**。这个文件被豁免了守卫，写进来的可渲染文案
//! 不会被拦下、也不会被翻译 —— 界面上要说的字一律在 `dialogs.rs` 里走 `tr(cx, …)`。
//! 表本身的形态由 [`every_alias_is_reachable`] 钉住。
//!
//! 归一层 [`normalize`] 去掉空白 / `_` / `-` / `/` / 括号并转小写，于是
//! `任务标题` / `Due Date` / `due_date` / `DUE-DATE` 落到同一个键上。

use duck_project::{Priority, Quadrant, TaskStatus};

use super::import::Field;

/// 列名的比对键：转小写 + 丢掉空白与常见分隔符。
///
/// `to_lowercase` 而不是 `to_ascii_lowercase`：土耳其语之外的全角字母、希腊字母
/// 都不在我们的表里，但用户可能写出 `ＰＲＩＯＲＩＴＹ`（全角）—— 全角字母的
/// `to_lowercase` 会把它折成半角小写，`to_ascii_lowercase` 不会。
pub(crate) fn normalize(header: &str) -> String {
    header
        .trim()
        .to_lowercase()
        .chars()
        .filter(|ch| {
            !matches!(
                ch,
                ' ' | '\t' | '_' | '-' | '/' | '\\' | '(' | ')' | '（' | '）'
            )
        })
        .collect()
}

/// 列名 → 字段。
///
/// 认不出返回 `None`（该列在预览里标「未使用」）。**不做模糊匹配**：`contains`
/// 之类的兜底会让 `创建时间` 撞上 `时间`、把一列时间戳当成截止时刻导进去，
/// 而这种错误在预览里看不出来。
pub(crate) fn field_of_header(header: &str) -> Option<Field> {
    let key = normalize(header);
    if key.is_empty() {
        return None;
    }
    let found = TABLE.iter().find(|(alias, _)| *alias == key)?;
    Some(found.1)
}

/// 状态取值 → `TaskStatus`。认稳定键（`todo` …）与常见中文写法。
pub(crate) fn status_of(value: &str) -> Option<TaskStatus> {
    let key = normalize(value);
    let stable = TaskStatus::parse(&key);
    if stable.is_some() {
        return stable;
    }
    match key.as_str() {
        "未开始" | "待处理" | "待办事项" => Some(TaskStatus::Todo),
        "进行中" | "在做" | "开发中" => Some(TaskStatus::Doing),
        "阻塞" | "被阻塞" | "卡住" => Some(TaskStatus::Blocked),
        "完成" | "已完成" | "已做" | "已结束" | "closed" | "finished" => {
            Some(TaskStatus::Done)
        }
        "搁置" | "推迟" | "缓冲" => Some(TaskStatus::Deferred),
        "取消" | "已废弃" | "作废" => Some(TaskStatus::Cancelled),
        _ => None,
    }
}

/// 优先级取值 → `Priority`。
///
/// 认 `P0`–`P3`（大小写不敏感）与裸数字 `0`–`3`。⚠️ **裸数字按 `P` 的序号读**，
/// 不按「1 = 高」读 —— 本仓的刻度是 `P0` 最高，与导出侧一致；参考项目那套
/// 「1 高 2 中 3 低」的写法在表里补了中文别名（`高` / `中` / `低`）承接，
/// 于是两种习惯都不会被猜错。
pub(crate) fn priority_of(value: &str) -> Option<Priority> {
    let key = normalize(value);
    let stable = Priority::parse(&key);
    if stable.is_some() {
        return stable;
    }
    match key.as_str() {
        "0" => Some(Priority::P0),
        "1" => Some(Priority::P1),
        "2" => Some(Priority::P2),
        "3" => Some(Priority::P3),
        "最高" | "紧急" | "立即" => Some(Priority::P0),
        "高" | "偏高" => Some(Priority::P1),
        "中" | "普通" | "正常" => Some(Priority::P2),
        "低" | "偏低" | "不急" => Some(Priority::P3),
        _ => None,
    }
}

/// 四象限取值 → `Quadrant`。认 `Q1`–`Q4`、裸数字 `1`–`4` 与四种中文说法。
pub(crate) fn quadrant_of(value: &str) -> Option<Quadrant> {
    let key = normalize(value);
    let stable = Quadrant::parse(&key);
    if stable.is_some() {
        return stable;
    }
    match key.as_str() {
        "1" => Some(Quadrant::Q1),
        "2" => Some(Quadrant::Q2),
        "3" => Some(Quadrant::Q3),
        "4" => Some(Quadrant::Q4),
        "紧急重要" | "重要紧急" => Some(Quadrant::Q1),
        "重要不紧急" | "不紧急重要" => Some(Quadrant::Q2),
        "紧急不重要" | "不重要紧急" => Some(Quadrant::Q3),
        "不紧急不重要" | "都不紧急" | "都不" => Some(Quadrant::Q4),
        _ => None,
    }
}

/// 「是 / 否」一类的取值 → `bool`。认不出返回 `None`（保持原样，不静默当 `false`）。
pub(crate) fn bool_of(value: &str) -> Option<bool> {
    match normalize(value).as_str() {
        "true" | "yes" | "y" | "on" | "1" | "是" | "置顶" | "星标" | "收藏" | "★" => {
            Some(true)
        }
        "false" | "no" | "n" | "off" | "0" | "否" | "" => Some(false),
        _ => None,
    }
}

/// 列名别名表：`(归一化别名, 字段)`。
///
/// 顺序无关（查找是线性的 `find`），但同一字段的别名聚在一起便于核对。
/// 加别名只要不与其他字段撞键就行 —— 撞键时**先出现的赢**，另一列标「未使用」。
const TABLE: &[(&str, Field)] = &[
    ("title", Field::Title),
    ("name", Field::Title),
    ("task", Field::Title),
    ("taskname", Field::Title),
    ("tasktitle", Field::Title),
    ("summary", Field::Title),
    ("subject", Field::Title),
    ("标题", Field::Title),
    ("任务", Field::Title),
    ("任务名", Field::Title),
    ("任务标题", Field::Title),
    ("主题", Field::Title),
    ("project", Field::Project),
    ("projectname", Field::Project),
    ("项目", Field::Project),
    ("项目名", Field::Project),
    ("所属项目", Field::Project),
    ("status", Field::Status),
    ("state", Field::Status),
    ("状态", Field::Status),
    ("priority", Field::Priority),
    ("prio", Field::Priority),
    ("优先级", Field::Priority),
    ("quadrant", Field::Quadrant),
    ("quad", Field::Quadrant),
    ("象限", Field::Quadrant),
    ("四象限", Field::Quadrant),
    ("tag", Field::Tags),
    ("tags", Field::Tags),
    ("标签", Field::Tags),
    ("start", Field::Start),
    ("startdate", Field::Start),
    ("startat", Field::Start),
    ("begin", Field::Start),
    ("beginat", Field::Start),
    ("开始", Field::Start),
    ("开始日期", Field::Start),
    ("开始时间", Field::Start),
    ("起始日期", Field::Start),
    ("计划开始", Field::Start),
    ("due", Field::Due),
    ("duedate", Field::Due),
    ("dueat", Field::Due),
    ("deadline", Field::Due),
    ("end", Field::Due),
    ("enddate", Field::Due),
    ("截止", Field::Due),
    ("截止日", Field::Due),
    ("截止日期", Field::Due),
    ("到期", Field::Due),
    ("到期日", Field::Due),
    ("结束日期", Field::Due),
    ("time", Field::Time),
    ("duetime", Field::Time),
    ("时刻", Field::Time),
    ("时间", Field::Time),
    ("截止时间", Field::Time),
    ("截止时刻", Field::Time),
    ("progress", Field::Progress),
    ("percent", Field::Progress),
    ("percentage", Field::Progress),
    ("进度", Field::Progress),
    ("完成度", Field::Progress),
    ("estimate", Field::Estimate),
    ("estimatehours", Field::Estimate),
    ("estimatedhours", Field::Estimate),
    ("预估", Field::Estimate),
    ("预估工时", Field::Estimate),
    ("预计工时", Field::Estimate),
    ("actual", Field::Actual),
    ("actualhours", Field::Actual),
    ("实际", Field::Actual),
    ("实际工时", Field::Actual),
    ("starred", Field::Starred),
    ("star", Field::Starred),
    ("pinned", Field::Starred),
    ("favorite", Field::Starred),
    ("favourite", Field::Starred),
    ("置顶", Field::Starred),
    ("星标", Field::Starred),
    ("收藏", Field::Starred),
    ("description", Field::Description),
    ("desc", Field::Description),
    // 自家 JSON 导出用的是 `descriptionMd`（正文是 Markdown）—— 少了这一条，
    // 「导出再导入」就会把描述整段丢掉，而且预览里看不出来。
    ("descriptionmd", Field::Description),
    ("descmd", Field::Description),
    ("detail", Field::Description),
    ("details", Field::Description),
    ("描述", Field::Description),
    ("任务描述", Field::Description),
    ("详情", Field::Description),
    ("说明", Field::Description),
    ("note", Field::Note),
    ("notes", Field::Note),
    ("remark", Field::Note),
    ("remarks", Field::Note),
    ("memo", Field::Note),
    ("备注", Field::Note),
    ("注释", Field::Note),
];

#[cfg(test)]
mod tests {
    use super::{TABLE, bool_of, field_of_header, normalize, priority_of, quadrant_of, status_of};
    use crate::pages::projects::import::Field;
    use duck_project::{Priority, Quadrant, TaskStatus};

    /// 归一化把常见的分隔符与大小写差异抹平。
    #[test]
    fn normalize_folds_case_and_separators() {
        assert_eq!(normalize("  Due Date "), "duedate");
        assert_eq!(normalize("due_date"), "duedate");
        assert_eq!(normalize("DUE-DATE"), "duedate");
        assert_eq!(normalize("due/date"), "duedate");
        assert_eq!(normalize("预估工时（小时）"), "预估工时小时");
    }

    /// 导出侧写的表头与参考项目的中文表头都能认出来 —— 这两组是**导入的主用例**。
    #[test]
    fn our_own_export_header_and_chinese_headers_both_map() {
        // 我们自己导出的 CSV 表头（`export.rs::csv` 的 `HEAD`）。
        let ours = "id,title,project,status,priority,quadrant,tags,start,due,progress";
        let mapped: Vec<Option<Field>> = ours.split(',').map(field_of_header).collect();
        assert_eq!(
            mapped,
            [
                None, // id 不导入：跨库搬 id 只会撞上本地已有的主键
                Some(Field::Title),
                Some(Field::Project),
                Some(Field::Status),
                Some(Field::Priority),
                Some(Field::Quadrant),
                Some(Field::Tags),
                Some(Field::Start),
                Some(Field::Due),
                Some(Field::Progress),
            ],
            "自家表头必须能原样导回"
        );

        // 参考项目 `TaskImport.vue` 里的 Excel 列名。
        let theirs: Vec<Option<Field>> = [
            "任务标题",
            "任务描述",
            "优先级",
            "状态",
            "预估工时",
            "实际工时",
            "Git分支",
            "创建时间",
            "开始时间",
            "完成时间",
            "备注",
        ]
        .iter()
        .map(|header| field_of_header(header))
        .collect();
        assert_eq!(theirs[0], Some(Field::Title));
        assert_eq!(theirs[1], Some(Field::Description));
        assert_eq!(theirs[2], Some(Field::Priority));
        assert_eq!(theirs[3], Some(Field::Status));
        assert_eq!(theirs[4], Some(Field::Estimate));
        assert_eq!(theirs[5], Some(Field::Actual));
        assert_eq!(theirs[8], Some(Field::Start), "开始时间 → 开始");
        assert_eq!(theirs[10], Some(Field::Note));
        // 本仓还没有的字段（批 7 的 git 分支 / 创建时间）认不出，标「未使用」。
        assert_eq!(theirs[6], None, "Git分支：本仓无此字段");
        assert_eq!(theirs[7], None, "创建时间：本仓无此字段");
        assert_eq!(theirs[9], None, "完成时间：本仓无此字段");
    }

    /// 认不出的列名一律 `None`，不做模糊匹配。
    #[test]
    fn unknown_headers_are_not_guessed() {
        for header in [
            "",
            "   ",
            "id",
            "创建时间",
            "Git分支",
            "随便一列",
            "status_x",
        ] {
            assert_eq!(field_of_header(header), None, "{header:?} 不该认出来");
        }
    }

    /// 别名表里不能有重复键 —— 撞键会让后一条永远取不到（`find` 返回第一条）。
    #[test]
    fn every_alias_is_reachable() {
        let mut seen = std::collections::HashSet::new();
        for (alias, field) in TABLE {
            assert!(
                seen.insert(*alias),
                "{alias:?}（{field:?}）撞了别的别名：后一条永远取不到"
            );
            assert_eq!(
                normalize(alias),
                *alias,
                "{alias:?} 没归一化，用户文件里的写法对不上"
            );
            assert!(
                field_of_header(alias).is_some(),
                "{alias:?} 在表里却查不出来"
            );
        }
    }

    /// 状态：稳定键与中文写法都认，认不出的返回 `None`。
    #[test]
    fn status_aliases_cover_stable_keys_and_chinese() {
        assert_eq!(status_of("todo"), Some(TaskStatus::Todo));
        assert_eq!(status_of("DONE"), Some(TaskStatus::Done));
        assert_eq!(status_of("进行中"), Some(TaskStatus::Doing));
        assert_eq!(status_of("已完成"), Some(TaskStatus::Done));
        assert_eq!(status_of("阻塞"), Some(TaskStatus::Blocked));
        assert_eq!(status_of("推迟"), Some(TaskStatus::Deferred));
        assert_eq!(status_of("已废弃"), Some(TaskStatus::Cancelled));
        assert_eq!(status_of("不知道啥状态"), None);
    }

    /// 优先级：`P` 形式、裸数字、中文三档都认。
    #[test]
    fn priority_aliases_cover_p_forms_digits_and_chinese() {
        assert_eq!(priority_of("P0"), Some(Priority::P0));
        assert_eq!(priority_of("p3"), Some(Priority::P3));
        assert_eq!(priority_of("2"), Some(Priority::P2), "裸数字按 P 序号读");
        assert_eq!(priority_of("高"), Some(Priority::P1));
        assert_eq!(priority_of("紧急"), Some(Priority::P0));
        assert_eq!(priority_of("低"), Some(Priority::P3));
        assert_eq!(priority_of("P9"), None, "越界的 P 不是优先级");
    }

    /// 四象限：`Q` 形式与四种中文说法。
    #[test]
    fn quadrant_aliases_cover_q_forms_and_chinese() {
        assert_eq!(quadrant_of("Q1"), Some(Quadrant::Q1));
        assert_eq!(quadrant_of("q4"), Some(Quadrant::Q4));
        assert_eq!(quadrant_of("3"), Some(Quadrant::Q3));
        assert_eq!(quadrant_of("重要不紧急"), Some(Quadrant::Q2));
        assert_eq!(quadrant_of("都不"), Some(Quadrant::Q4));
        assert_eq!(quadrant_of("Q5"), None);
    }

    /// 布尔：空串当 `false`（空单元格就是「没勾」），认不出的返回 `None`。
    #[test]
    fn bool_aliases_treat_blank_as_false_and_junk_as_unknown() {
        assert_eq!(bool_of("true"), Some(true));
        assert_eq!(bool_of("收藏"), Some(true));
        assert_eq!(bool_of(""), Some(false), "空单元格 = 没勾");
        assert_eq!(bool_of("否"), Some(false));
        assert_eq!(bool_of("或许"), None, "认不出就别硬当 false");
    }
}
