//! 清单 `[io]` 段：声明式输入 / 参数 / 输出契约（API 版本 2 起支持）。
//!
//! 设计见 `docs/ifm-plugin-port-plan.md` ADR-1：插件只声明「要什么输入、
//! 有哪些参数、产出什么形状」，宿主用**一套**通用执行页渲染。声明即契约——
//! 宿主不读插件代码，未来 process 轨的运行请求也直接由这份声明生成。
//!
//! 与清单其余部分一样走 `deny_unknown_fields`：拼写漂移在解析期报错，
//! 而不是运行期静默丢字段。所有列表字段 `#[serde(default)]` ⇒ 老清单零改动。
//!
//! ```toml
//! [[tools]]
//! id = "duck.text.formatter"          # 一工具一插件：与插件 id 相同
//! name = "格式化"
//!
//! [tools.io]
//! allow_files = "read"
//!
//! [[tools.io.fields]]
//! id = "input"
//! kind = "textarea"
//! label = "输入"
//! required = true
//!
//! [[tools.io.params]]
//! id = "lang"
//! kind = "select"
//! label = "语言"
//! default = "auto"
//! options = [{ value = "auto", label = "自动识别" }]
//!
//! [[tools.io.output]]
//! kind = "code"
//! label = "结果"
//! ```

use serde::Deserialize;

use crate::PluginError;

/// 支持 `[io]` 的最低清单 API 版本。
///
/// 旧宿主对带 `[io]` 的清单会因 `deny_unknown_fields` 直接拒绝解析；
/// 抬版本号让这个不兼容**在安装期**显式暴露，而不是运行期静默。
pub const IO_MIN_API_VERSION: u32 = 2;

/// 交给**独立进程插件**的私有数据目录绝对路径（环境变量名）。
///
/// 语义是 **JS 轨 `duck.dataDir` 的进程版**：两条轨道给的是同一个目录
/// （`<app_data>/plugin-data/<plugin_id>`），只是传输层不同——沙箱全局 vs 环境变量。
/// 所以这条不引入新能力，它只是让 process 轨也能用上既有的那份。
///
/// ⚠️ 跨进程对外协议：插件在别的进程、由别人写，改名**没有任何编译错误**，
/// 只会表现成「插件忽然存不下东西」。`env_name_is_pinned` 把字面量钉死。
///
/// 缺席 = 宿主没给（老宿主 / 目录建不出来）。插件当它不存在处理，**不要自己猜路径**：
/// 猜出来的路径一定指不到宿主认可的那个目录，写进去的东西下次就找不到了。
pub const ENV_DATA_DIR: &str = "DUCK_DATA_DIR";

/// 一个工具的完整 IO 契约（清单里的 `[tools.io]` 段）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IoSpec {
    /// 输入字段（通常是那块主输入框）。0~2 个；更多输入请改用 params。
    #[serde(default)]
    pub fields: Vec<FieldSpec>,
    /// 参数项（表单控件），按声明顺序渲染。
    #[serde(default)]
    pub params: Vec<ParamSpec>,
    /// 输出形状；缺省 = 纯文本输出。
    #[serde(default)]
    pub output: Option<OutputSpec>,
    /// 运行时文件授权请求：`none` / `read` / `read_write`。
    ///
    /// 注意这只描述**用户在本次运行里选择的路径**的用途；插件自身数据目录的
    /// 读写仍由 `[capabilities]` 的 glob 管，两者语义不同。
    #[serde(default)]
    pub allow_files: FileAccess,
    /// 页面布局；缺省 = 通用执行页。
    #[serde(default)]
    pub layout: IoLayout,
    /// 两个输入字段的排布；缺省上下堆叠。
    ///
    /// `row` 是「A/B 对照」语义（配置比较、编码转换、JSON Schema 校验）：
    /// 左右各一个输入框，中间的关系一眼可见——堆成上下之后用户就得自己在
    /// 脑子里拼。
    #[serde(default)]
    pub fields_layout: FieldsLayout,
    /// 并排双栏顶部是否给「交换」按钮（把 A、B 两个输入互换）。
    ///
    /// 只对 `fields_layout = "row"` + 恰好 2 个字段有意义：单向转换（编码 A → B）
    /// 试反了方向，用户的第一反应就是「换过来再试一次」。
    #[serde(default)]
    pub swap_fields: bool,
    /// 主操作按钮的文案（中文源串；空 = 宿主缺省「运行」，经插件表取词）。
    ///
    /// 只在 `layout = "job"` 下渲染——通用执行页的按钮统一叫「运行」，
    /// 每个插件各说各话反而破坏一致性。
    #[serde(default)]
    pub run_label: String,
    /// 日志栏的空态提示（中文源串；空 = 宿主缺省文案，经插件表取词）。
    ///
    /// 只在 `layout = "job"` 下渲染：长任务页的日志栏**恒在**，没跑之前
    /// 用这句话告诉用户先做什么（参考原版的「选择包目录后点击开始合并」）。
    #[serde(default)]
    pub log_hint: String,
    /// 页内扫描（缺省无）：宿主据此渲染「重新扫描」按钮，并按约定回填
    /// 只读展示参数。
    ///
    /// 扫描 = 宿主以 `action = "scan"` 再拉一次插件二进制；插件返回
    /// `{"versions": ["a.zip", "b.zip", …]}` 形状的 body。回填约定（声明序）：
    /// 第 1 个 `display` 参数 ← 逗号连接的完整列表，第 2 个 ← 「首 → 末」区间，
    /// 其余不动。展示参数是**只读**的，不存在「改了却不生效」的误导。
    #[serde(default)]
    pub scan: Option<ScanSpec>,
    /// 工作台配置（`layout = "workbench"` 时必填）。
    #[serde(default)]
    pub workbench: Option<WorkbenchSpec>,
}

/// 工作台的左列表行为（`[io.workbench]`）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkbenchSpec {
    /// 点中列表项后回填的参数 id（`select` / `text` / `pills`）。
    ///
    /// 回填就是「用户点了一次选择」：该参数照常参与运行请求，隐藏逻辑
    /// （`visible_when`）也照常生效——列表只是这个参数的**可视形态**。
    pub target: String,
}

/// 执行页布局。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IoLayout {
    /// 通用执行页（缺省）：参数行内条 + 运行按钮 + 左输入 / 右结果双栏。
    #[default]
    Default,
    /// 长任务页（原型 D）：左侧垂直分组配置 + 进度 + 统计卡，
    /// 右侧固定日志栏（时间戳 + 级别着色 + 空态提示）。
    ///
    /// 给「跑几分钟、日志几百行」的工具用：合并、批量比对这类。
    /// 参数在此布局下**垂直整行**排布（`group` 相同的相邻参数共一个节标题）。
    Job,
    /// 工作台页（原型 F）：左侧条目列表 + 右侧完整执行区。
    ///
    /// 给「原版本身是一个小应用」的工具用：速查手册（左表列表 + 右查询）、
    /// PDM 阅读器（左表列表 + 右字段）这类。**只做骨架**——历史 / 回收站 /
    /// 审计那些应用级功能不在契约里。
    ///
    /// 左侧列表的数据来自 [`IoSpec::scan`]（`action = "scan"`，body 形状
    /// `{"list": ["a", "b"]}` 或 `{"list": [{"value": …, "label": …}]}`）；
    /// 点中一项就回填 [`IoSpec::workbench`] 声明的那个参数。列表空态 / 扫描失败
    /// 都不是错误——右侧执行区照常可用，列表只是导航。
    Workbench,
}

/// 两个输入字段的排布。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldsLayout {
    /// 上下堆叠（缺省）。
    #[default]
    Stack,
    /// 左右并排，各占一半。
    Row,
}

/// 页内扫描声明。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanSpec {
    /// 扫描哪个目录：一个 `kind = "path"`、`pick = "folder"` 的参数 id。
    pub src: String,
}

/// 输入字段：执行页底部（沉底）的那块主输入区。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    /// 字段稳定 id（运行请求里的键名）。
    pub id: String,
    /// 控件种类。
    pub kind: FieldKind,
    /// 标签（中文源串，渲染期经插件表取词）。
    pub label: String,
    /// 占位文案（中文源串，可省略）。
    #[serde(default)]
    pub placeholder: String,
    /// 是否必填；缺省可空。
    #[serde(default)]
    pub required: bool,
}

/// 输入字段种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    /// 单行文本。
    Text,
    /// 多行文本域（可拖入文件）。
    Textarea,
}

/// 参数项：执行区顶部（置顶）的表单控件。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    /// 参数稳定 id（运行请求里的键名）。
    pub id: String,
    /// 控件种类。
    pub kind: ParamKind,
    /// 标签（中文源串）。
    pub label: String,
    /// 缺省值（字符串形态；`switch` 用 `"true"` / `"false"`）。
    #[serde(default)]
    pub default: String,
    /// 占位文案（仅 `text`）。
    #[serde(default)]
    pub placeholder: String,
    /// 是否必填；缺省可空（空值时宿主拒绝执行并提示）。
    #[serde(default)]
    pub required: bool,
    /// `select` 的选项（必填）；`range` 的两段标签复用 `label` + `min_label`/`max_label`。
    #[serde(default)]
    pub options: Vec<ParamOption>,
    /// `number` / `range` 下界。
    #[serde(default)]
    pub min: Option<f64>,
    /// `number` / `range` 上界。
    #[serde(default)]
    pub max: Option<f64>,
    /// `path` 参数弹出的原生面板种类：`file`（缺省）/ `folder`。
    ///
    /// 手填路径永远可用；这个字段只决定「选择…」按钮打开哪种面板。
    /// 声明 `path` 参数的工具必须同时给出 `[tools.io] allow_files`——
    /// 没要权限却要路径是**声明自相矛盾**，在装载期就报错。
    #[serde(default)]
    pub pick: PathPick,
    /// 条件显示：`{ 另一个参数的 id = 期望值 }`，全部命中才显示这个参数。
    ///
    /// 只对 `select` / `switch` 做条件（别的控件没有「当前值是一个枚举」的语义）。
    /// 被引用的参数必须**声明在它前面**——这条限制顺带把环形依赖挡在装载期，
    /// 不需要额外的图检查。隐藏的参数**同时**从运行请求里消失，插件不会收到
    /// 「用户没见过的旋钮」的值。
    ///
    /// 用途：一个工具多种模式时，别把 6 个旋钮一次性摊在用户面前——
    /// 与「操作区置顶、干净」的布局铁律同一个取舍。
    #[serde(default)]
    pub visible_when: Option<std::collections::BTreeMap<String, String>>,
    /// 小节标题（中文源串；空 = 不带标题，紧跟上一个参数）。
    ///
    /// 只在 `layout = "job"` 下渲染：相邻参数的 `group` 相同就共一个节标题
    /// （如「合并选项」下的一排开关），变了才起新节。通用执行页忽略它。
    #[serde(default)]
    pub group: String,
    /// 渲染风格；缺省按控件本身画。
    ///
    /// `tabs` 只对 `select` / `pills` 有意义：把这一个参数画成页面顶部的分段
    /// 页签（参考项目 `AppSegmented`），并从参数条里移除。它配合
    /// `visible_when` 使用——同一个工具多种模式时，用户看到的是「一页一个模式」，
    /// 而不是一个下拉 + 一堆时隐时现的旋钮。
    #[serde(default)]
    pub style: ParamStyle,
}

/// 参数的渲染风格。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamStyle {
    /// 按控件本身画（缺省）。
    #[default]
    Plain,
    /// 页面顶部的分段页签（仅 `select` / `pills`）。
    Tabs,
}

/// `path` 参数的选取粒度。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathPick {
    /// 选一个文件（缺省）。
    #[default]
    File,
    /// 选一个目录。
    Folder,
}

/// 参数控件种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    /// 单行文本。
    Text,
    /// 路径选择（文件或目录，宿主弹出原生面板并做运行时授权）。
    Path,
    /// 分段 / 下拉选择，必须给 `options`。
    Select,
    /// 药丸按钮组（`select` 的视觉变体）：候选 ≤ 6 个、每个都值得直接点出来时用，
    /// 提交值与 `select` 完全一致。候选多请用 `select`。
    Pills,
    /// 只读展示（不参与运行请求）。`layout = "job"` 下由扫描回填
    /// （见 [`IoSpec::scan`] 的回填约定）。
    Display,
    /// 开关。
    Switch,
    /// 数值输入。
    Number,
    /// 区间（两个数值输入）。
    Range,
}

/// `select` 的一个选项。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamOption {
    /// 提交给插件的值。
    pub value: String,
    /// 展示名（中文源串）。
    pub label: String,
}

/// 输出形状声明。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    /// 渲染器种类。
    pub kind: OutputKind,
    /// 标签（中文源串，可省略 = 「结果」）。
    #[serde(default)]
    pub label: String,
    /// `code` 输出的语言提示（如 `"json"`、`"sql"`；可省略）。
    #[serde(default)]
    pub language: String,
    /// `table` 输出的列名（中文源串，按序）。
    #[serde(default)]
    pub columns: Vec<String>,
    /// `table` 输出里可就地编辑的列（省略 = 整表只读）。
    ///
    /// 声明了它，宿主才会在这些列上放输入控件，并把改动并入
    /// `params.__edits` 再跑一次（见 [`crate::table::EDIT_PARAM`]）。
    /// **没声明就不可能出现输入框**——这是「插件不读 `__edits` 却给了输入框」
    /// 这道谜题的唯一防线。
    #[serde(default)]
    pub cells: Vec<CellSpec>,
    /// `kind = "table"` 解不出表格时的退路（默认 `text`）。
    ///
    /// 存在的理由很具体：一个工具常常既有**表格**输出（接口清单）、又有**代码**
    /// 输出（生成的 Java 源码），而 `[io.output]` 只能声明一个 kind。声明
    /// `kind = "table"` 才拿得到表格，但代码动作也一并不能等宽了——把退路写成
    /// `code`，两条路各归各位（详见 `ToolRunView::run` 里的判定）。
    #[serde(default)]
    pub fallback: Option<OutputKind>,
}

impl OutputSpec {
    /// 校验可编辑列声明。
    ///
    /// 规则都指向同一件事：**声明了就必须能被兑现**。半生效的声明比没声明更糟
    /// ——用户会看到一列表头、改了字，然后重跑之后什么都没变。
    fn validate(&self) -> Result<(), PluginError> {
        // 退路只在「解析后才定形状」的输出上有意义：别的 kind 一开始就确定了。
        // `table`（表解不出来）与 `preview`（图解不出来 / 扩展名不认识）都是
        // 「这次运行才知道渲染什么」的形状。
        if let Some(fallback) = self.fallback {
            if !matches!(self.kind, OutputKind::Table | OutputKind::Preview) {
                return Err(PluginError::Invalid(
                    "[io.output] fallback only applies to kind = \"table\" or \"preview\"".into(),
                ));
            }
            if fallback == OutputKind::Table {
                return Err(PluginError::Invalid(
                    "[io.output] fallback must not be \"table\" (it is the table's fallback)"
                        .into(),
                ));
            }
        }
        if self.cells.is_empty() {
            return Ok(());
        }
        if self.kind != OutputKind::Table {
            return Err(PluginError::Invalid(format!(
                "[io.output] cells requires kind = \"table\" (got {:?})",
                self.kind
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for cell in &self.cells {
            if !seen.insert(cell.column.as_str()) {
                return Err(PluginError::Invalid(format!(
                    "[io.output] duplicate editable column {:?}",
                    cell.column
                )));
            }
            // 表头可能由插件在 body 里给（`columns` 留空是合法的），那种情况下
            // 装载期无法核对列名——只校验能校验的。
            if !self.columns.is_empty() && !self.columns.iter().any(|c| c == &cell.column) {
                return Err(PluginError::Invalid(format!(
                    "[io.output] editable column {:?} is not listed in columns",
                    cell.column
                )));
            }
            if cell.kind == CellKind::Select && cell.options.is_empty() {
                return Err(PluginError::Invalid(format!(
                    "[io.output] editable column {:?}: select requires options",
                    cell.column
                )));
            }
        }
        Ok(())
    }
}

/// `table` 输出里某一列的编辑控件。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellSpec {
    /// 列名；`[io.output] columns` 非空时必须是其中之一。
    pub column: String,
    /// 控件种类。
    pub kind: CellKind,
    /// `select` 的候选（`text` 忽略）。
    #[serde(default)]
    pub options: Vec<ParamOption>,
}

/// 单元格编辑控件种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CellKind {
    /// 单行文本。
    Text,
    /// 下拉选择（`options` 必填）。
    Select,
}

/// 输出渲染器种类（与宿主 `duck-components` 的结果渲染器一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputKind {
    /// 纯文本（可选中、可复制）。
    Text,
    /// 只读代码块 + 复制。
    Code,
    /// 结果表（排序 + 导出 CSV）。
    Table,
    /// 结构树 + 详情。
    Tree,
    /// A/B 双栏差异 + 统计。
    ///
    /// 两条供给方式（宿主都支持，优先级从高到低）：
    /// 1. process 轨发 `{"type":"body","chunk":"…"}`，拼起来是一份
    ///    [`crate::diff::DiffDoc`] 的 JSON；
    /// 2. `output` 里放 unified diff 文本（`--- / +++ / @@`，git 那种）。
    ///
    /// 两条都不成立时**退回纯文本渲染**，不当失败——差异渲染是增强，不是前置条件。
    /// 无论哪条路径，`output` 都要给一份可读的文本镜像：复制按钮复制的是它。
    Diff,
    /// 流式日志（自动滚底）。
    Log,
    /// 内联预览（图片 / SVG 文件）。
    ///
    /// process 轨发 `{"preview": "<文件绝对路径>"}` 形状的 body，宿主按扩展名
    /// 用 GPUI 的图片管线内联渲染——PNG / JPEG / GIF / WebP / BMP 走光栅解码，
    /// SVG 走 gpui 内置的 resvg 光栅化，两者都是**全彩**（宿主不额外引依赖）。
    ///
    /// 路径必须是插件**已经写盘**的文件：预览是读盘，不是传字节——几 MB 的
    /// PNG 塞进 NDJSON 既慢又占内存，而文件本来就在磁盘上。扩展名不被认识、
    /// 路径缺失或没发 body 时按 `fallback` 渲染（缺省 `text`），不当失败。
    /// `output` 文本仍然保留：复制 / 导出按钮复制的是它。
    Preview,
}

/// 运行时文件授权请求。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccess {
    /// 不需要用户文件。
    #[default]
    None,
    /// 只读用户选择的路径。
    Read,
    /// 读写用户选择的路径。
    ReadWrite,
}

impl IoSpec {
    /// 校验 IO 声明：id 唯一、select 必须有选项、default 必须命中选项、
    /// 数值边界有序。`api_version` 用于拦「v1 清单带 `[io]`」。
    ///
    /// # Errors
    /// 任一规则不满足即返回 [`PluginError::Invalid`]。
    pub fn validate(&self, api_version: u32) -> Result<(), PluginError> {
        if api_version < IO_MIN_API_VERSION {
            return Err(PluginError::Invalid(format!(
                "[io] requires api_version >= {IO_MIN_API_VERSION} (got {api_version})"
            )));
        }
        if self.fields.len() > 2 {
            return Err(PluginError::Invalid(
                "[io] at most 2 fields (more inputs belong in params)".into(),
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for field in &self.fields {
            Self::check_id(&field.id, &mut ids, "field")?;
        }
        for (index, param) in self.params.iter().enumerate() {
            Self::check_id(&param.id, &mut ids, "param")?;
            Self::check_visible_when(&self.params[..index], param)?;
            match param.kind {
                ParamKind::Select | ParamKind::Pills => {
                    if param.options.is_empty() {
                        return Err(PluginError::Invalid(format!(
                            "param {:?}: select requires options",
                            param.id
                        )));
                    }
                    let mut values = std::collections::HashSet::new();
                    for option in &param.options {
                        if option.value.trim().is_empty() {
                            return Err(PluginError::Invalid(format!(
                                "param {:?}: option value must not be empty",
                                param.id
                            )));
                        }
                        if !values.insert(option.value.as_str()) {
                            return Err(PluginError::Invalid(format!(
                                "param {:?}: duplicate option value {:?}",
                                param.id, option.value
                            )));
                        }
                    }
                    if !param.default.is_empty() && !values.contains(param.default.as_str()) {
                        return Err(PluginError::Invalid(format!(
                            "param {:?}: default {:?} is not one of the option values",
                            param.id, param.default
                        )));
                    }
                }
                ParamKind::Switch => {
                    if !(param.default.is_empty()
                        || param.default == "true"
                        || param.default == "false")
                    {
                        return Err(PluginError::Invalid(format!(
                            "param {:?}: switch default must be \"true\" or \"false\"",
                            param.id
                        )));
                    }
                }
                ParamKind::Number | ParamKind::Range => {
                    // 不用 `if let .. && ..`（let_chains 需 1.88，本仓 MSRV 1.85）。
                    if param.min.zip(param.max).is_some_and(|(min, max)| min > max) {
                        return Err(PluginError::Invalid(format!(
                            "param {:?}: min > max",
                            param.id
                        )));
                    }
                }
                ParamKind::Text => {}
                ParamKind::Display => {
                    // 展示参数是只读的：必填没有意义（永远填不了），选项更是无源之水。
                    if param.required {
                        return Err(PluginError::Invalid(format!(
                            "param {:?}: display params are read-only and cannot be required",
                            param.id
                        )));
                    }
                    if !param.options.is_empty() {
                        return Err(PluginError::Invalid(format!(
                            "param {:?}: display params cannot have options",
                            param.id
                        )));
                    }
                }
                ParamKind::Path => {
                    // 要路径就得要文件权限：两个声明必须同时出现，否则装载期
                    // 就把「面板给了路径、执行时才被权限挡下」这种运行期谜题掐掉。
                    if self.allow_files == FileAccess::None {
                        return Err(PluginError::Invalid(format!(
                            "param {:?}: path params require allow_files = \"read\" or \"read_write\"",
                            param.id
                        )));
                    }
                }
            }
        }
        // 分段页签只能有一个（页面顶部只有一行），且必须是一个枚举控件：
        // 页签的语义就是「当前模式」，没有取值集合的控件画不成页签。
        let tabs: Vec<&ParamSpec> = self
            .params
            .iter()
            .filter(|param| param.style == ParamStyle::Tabs)
            .collect();
        if tabs.len() > 1 {
            return Err(PluginError::Invalid(
                "[io] at most one param may use style = \"tabs\"".into(),
            ));
        }
        // 谓词并进 `filter` 而不是嵌套 if：`let` 链要 1.88，MSRV 是 1.85。
        if let Some(param) = tabs
            .first()
            .filter(|param| !matches!(param.kind, ParamKind::Select | ParamKind::Pills))
        {
            return Err(PluginError::Invalid(format!(
                "param {:?}: style = \"tabs\" requires a select or pills param",
                param.id
            )));
        }
        if self.swap_fields && (self.fields.len() != 2 || self.fields_layout != FieldsLayout::Row) {
            return Err(PluginError::Invalid(
                "[io] swap_fields requires exactly 2 fields with fields_layout = \"row\"".into(),
            ));
        }
        // 工作台：骨架要有「列表 → 回填哪个参数」与「列表从哪来」两件事。
        // 缺一就渲染不出左栏，装载期直接拒绝。先于 scan 校验：`[io.workbench]`
        // 放错布局时该报的是「放错」，不是更深处的 scan 规则。
        if self.layout == IoLayout::Workbench {
            let Some(workbench) = &self.workbench else {
                return Err(PluginError::Invalid(
                    "[io] layout = \"workbench\" requires a [io.workbench] section".into(),
                ));
            };
            if self.scan.is_none() {
                return Err(PluginError::Invalid(
                    "[io] layout = \"workbench\" requires scan (the list source)".into(),
                ));
            }
            // 目标参数必须存在，且是「点一下就有意义」的取值型控件。
            let target = self
                .params
                .iter()
                .find(|param| param.id == workbench.target)
                .filter(|param| {
                    matches!(
                        param.kind,
                        ParamKind::Select | ParamKind::Pills | ParamKind::Text
                    )
                });
            if target.is_none() {
                return Err(PluginError::Invalid(format!(
                    "[io.workbench] target {:?} must be a declared select / pills / text param",
                    workbench.target
                )));
            }
        } else if self.workbench.is_some() {
            return Err(PluginError::Invalid(
                "[io.workbench] requires layout = \"workbench\"".into(),
            ));
        }
        if let Some(scan) = &self.scan {
            let Some(param) = self.params.iter().find(|param| param.id == scan.src) else {
                return Err(PluginError::Invalid(format!(
                    "[io] scan.src {:?} is not a declared param",
                    scan.src
                )));
            };
            if param.kind != ParamKind::Path {
                return Err(PluginError::Invalid(format!(
                    "[io] scan.src {:?} must be a path param",
                    scan.src
                )));
            }
            let displays = self
                .params
                .iter()
                .filter(|param| param.kind == ParamKind::Display)
                .count();
            // 工作台布局的扫描喂的是左列表（`[io.workbench]`），不回填 display。
            if displays == 0 && self.layout != IoLayout::Workbench {
                return Err(PluginError::Invalid(
                    "[io] scan requires at least one display param to fill".into(),
                ));
            }
        }
        if let Some(output) = &self.output {
            output.validate()?;
        }
        Ok(())
    }

    /// 校验条件显示：被引用的参数必须**更早声明**（顺带排除环形依赖），
    /// 且只能是 `select` / `switch`，期望值必须是它的合法取值之一。
    fn check_visible_when(earlier: &[ParamSpec], param: &ParamSpec) -> Result<(), PluginError> {
        let Some(conditions) = param.visible_when.as_ref() else {
            return Ok(());
        };
        if conditions.is_empty() {
            return Err(PluginError::Invalid(format!(
                "param {:?}: visible_when must not be empty (drop the key instead)",
                param.id
            )));
        }
        for (key, expected) in conditions {
            let Some(guard) = earlier.iter().find(|other| &other.id == key) else {
                return Err(PluginError::Invalid(format!(
                    "param {:?}: visible_when references {:?}, which is not a param declared before it",
                    param.id, key
                )));
            };
            match guard.kind {
                ParamKind::Switch => {
                    if expected != "true" && expected != "false" {
                        return Err(PluginError::Invalid(format!(
                            "param {:?}: visible_when[{:?}] must be \"true\" or \"false\" for a switch",
                            param.id, key
                        )));
                    }
                }
                ParamKind::Select | ParamKind::Pills => {
                    if !guard.options.iter().any(|option| &option.value == expected) {
                        return Err(PluginError::Invalid(format!(
                            "param {:?}: visible_when[{:?}] = {:?} is not one of its option values",
                            param.id, key, expected
                        )));
                    }
                }
                _ => {
                    return Err(PluginError::Invalid(format!(
                        "param {:?}: visible_when[{:?}] must guard a select or switch param",
                        param.id, key
                    )));
                }
            }
        }
        Ok(())
    }

    fn check_id<'a>(
        id: &'a str,
        ids: &mut std::collections::HashSet<&'a str>,
        kind: &str,
    ) -> Result<(), PluginError> {
        if id.trim().is_empty() {
            return Err(PluginError::Invalid(format!("{kind} id must not be empty")));
        }
        if !ids.insert(id) {
            return Err(PluginError::Invalid(format!("duplicate {kind} id {id:?}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 数据目录的环境变量名同样是**跨进程的对外协议**——插件编译不到它，
    /// 改名字只会表现成「插件忽然存不下东西」。改名先把这条测试改红。
    ///
    /// 放在这里而不是 `i18n.rs` 的 `env_names_are_pinned`：那一条钉的是取词契约，
    /// 名字相近但管的是另一件事，混在一起会让「改哪条」变得靠猜。
    #[test]
    fn data_dir_env_name_is_pinned() {
        assert_eq!(ENV_DATA_DIR, "DUCK_DATA_DIR");
    }

    fn parse(source: &str) -> Result<crate::PluginManifest, crate::PluginError> {
        crate::PluginManifest::parse(source)
    }

    const HEADER: &str = r#"
id = "dev.duck.sample"
name = "示例"
version = "0.1.0"
execution_model = "js"
"#;

    #[test]
    fn api_v1_with_io_is_rejected() {
        let source = format!(
            r#"{HEADER}api_version = 1
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.fields]]
id = "input"
kind = "textarea"
label = "输入"
"#
        );
        let error = parse(&source).unwrap_err();
        assert!(error.to_string().contains("api_version"), "{error}");
    }

    #[test]
    fn select_requires_options() {
        let source = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "lang"
kind = "select"
label = "语言"
"#
        );
        let error = parse(&source).unwrap_err();
        assert!(
            error.to_string().contains("select requires options"),
            "{error}"
        );
    }

    #[test]
    fn default_must_hit_an_option() {
        let source = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "lang"
kind = "select"
label = "语言"
default = "auto"
options = [{{ value = "json", label = "JSON" }}]
"#
        );
        let error = parse(&source).unwrap_err();
        assert!(
            error.to_string().contains("not one of the option values"),
            "{error}"
        );
    }

    #[test]
    fn duplicate_ids_across_fields_and_params() {
        let source = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.fields]]
id = "input"
kind = "textarea"
label = "输入"
[[tools.io.params]]
id = "input"
kind = "text"
label = "输入"
"#
        );
        let error = parse(&source).unwrap_err();
        assert!(error.to_string().contains("duplicate param id"), "{error}");
    }

    #[test]
    fn unknown_io_field_is_rejected() {
        let source = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
typo = true
"#
        );
        assert!(parse(&source).is_err());
    }

    #[test]
    fn full_spec_roundtrip() {
        let source = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
group = "文本"
beta = false
dev = false
[tools.io]
allow_files = "read"
[[tools.io.fields]]
id = "input"
kind = "textarea"
label = "输入"
placeholder = "粘贴…"
required = true
[[tools.io.params]]
id = "lang"
kind = "select"
label = "语言"
default = "auto"
options = [{{ value = "auto", label = "自动" }}, {{ value = "json", label = "JSON" }}]
[[tools.io.params]]
id = "minify"
kind = "switch"
label = "压缩输出"
default = "false"
[[tools.io.params]]
id = "depth"
kind = "number"
label = "缩进"
min = 0.0
max = 8.0
[tools.io.output]
kind = "code"
label = "结果"
language = "json"
"#
        );
        let manifest = parse(&source).expect("valid manifest");
        let tool = &manifest.tools[0];
        assert_eq!(tool.group.as_deref(), Some("文本"));
        assert!(!tool.beta);
        let io = tool.io.as_ref().expect("io present");
        assert_eq!(io.allow_files, FileAccess::Read);
        assert_eq!(io.fields[0].kind, FieldKind::Textarea);
        assert_eq!(io.params.len(), 3);
        assert_eq!(io.output.as_ref().unwrap().kind, OutputKind::Code);
        io.validate(manifest.api_version).expect("valid io");
    }

    #[test]
    fn v1_manifest_without_io_still_loads() {
        let source = r#"
id = "dev.duck.sample"
name = "示例"
version = "0.1.0"
api_version = 1
execution_model = "js"
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
"#;
        parse(source).expect("v1 without io must keep loading");
    }

    #[test]
    fn path_param_without_file_access_is_rejected() {
        let source = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "src_dir"
kind = "path"
label = "源目录"
pick = "folder"
"#
        );
        let error = parse(&source).unwrap_err();
        assert!(
            error.to_string().contains("allow_files"),
            "path param must demand file access: {error}"
        );
    }

    #[test]
    fn path_pick_defaults_to_file_and_accepts_folder() {
        let source = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
allow_files = "read_write"
[[tools.io.params]]
id = "src_file"
kind = "path"
label = "源文件"
[[tools.io.params]]
id = "src_dir"
kind = "path"
label = "源目录"
pick = "folder"
"#
        );
        let manifest = parse(&source).expect("valid manifest");
        let io = manifest.tools[0].io.as_ref().expect("io present");
        // 不写 `pick` 就是选文件：多数工具要的是单个文件。
        assert_eq!(io.params[0].pick, PathPick::File);
        assert_eq!(io.params[1].pick, PathPick::Folder);

        io.validate(manifest.api_version).expect("valid io");
    }

    /// 可编辑列：正常解析 + 三种「声明了却兑现不了」的拒绝。
    #[test]
    fn editable_cells_parse_and_are_validated() {
        let ok = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io.output]
kind = "table"
columns = ["函数", "字段", "类型"]
[[tools.io.output.cells]]
column = "类型"
kind = "select"
options = [{{ value = "string", label = "字符串" }}, {{ value = "amount", label = "金额" }}]
[[tools.io.output.cells]]
column = "字段"
kind = "text"
"#
        );
        let manifest = parse(&ok).expect("valid manifest");
        let io = manifest.tools[0].io.as_ref().expect("io present");
        let output = io.output.as_ref().expect("output present");
        assert_eq!(output.cells.len(), 2);
        assert_eq!(output.cells[0].kind, CellKind::Select);
        assert_eq!(output.cells[0].options.len(), 2);
        assert_eq!(output.cells[1].kind, CellKind::Text);
        io.validate(manifest.api_version).expect("valid io");

        // ① 不是 table 却声明了可编辑列：画不出来，装载期就拒绝。
        let not_table = ok.replace("kind = \"table\"", "kind = \"text\"");
        let error = parse(&not_table).unwrap_err();
        assert!(
            error.to_string().contains("kind = \"table\""),
            "cells on a non-table output must be rejected: {error}"
        );

        // ② 列名不在 columns 里：用户会看到一列不存在的东西。
        let wrong_column = ok.replace("column = \"字段\"", "column = \"不存在\"");
        let error = parse(&wrong_column).unwrap_err();
        assert!(
            error.to_string().contains("not listed in columns"),
            "an editable column must exist in columns: {error}"
        );

        // ③ select 没给选项：等于让用户对着一列空白发呆。
        let no_options = ok.replace(
            "options = [{ value = \"string\", label = \"字符串\" }, { value = \"amount\", label = \"金额\" }]",
            "",
        );
        let error = parse(&no_options).unwrap_err();
        assert!(
            error.to_string().contains("select requires options"),
            "a select cell needs options: {error}"
        );
    }

    /// `preview` 输出与退路：fallback 只允许跟在 `table` / `preview` 后面。
    #[test]
    fn preview_output_accepts_fallback_but_other_kinds_do_not() {
        let ok = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[tools.io.output]
kind = "preview"
fallback = "text"
label = "图表"
"#
        );
        let manifest = parse(&ok).expect("preview with fallback is valid");
        let io = manifest.tools[0].io.as_ref().expect("io present");
        assert_eq!(io.output.as_ref().unwrap().kind, OutputKind::Preview);

        // 代码输出一开始就确定了形状，没有「解不出来」可言。
        let on_code = ok.replace("kind = \"preview\"", "kind = \"code\"");
        let error = parse(&on_code).unwrap_err();
        assert!(error.to_string().contains("fallback"), "{error}");
    }

    /// 工作台声明：必须有 `[io.workbench]`（target 指向已声明的取值型参数）、
    /// 必须有 scan（列表来源）；反之 `[io.workbench]` 不能脱离该布局存在。
    #[test]
    fn workbench_layout_is_validated() {
        let body = |extra: &str| {
            format!(
                r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
allow_files = "read"
layout = "workbench"
scan = {{ src = "src_dir" }}
{extra}
[[tools.io.params]]
id = "src_dir"
kind = "path"
label = "目录"
pick = "folder"
[[tools.io.params]]
id = "sheet"
kind = "select"
label = "表"
options = [{{ value = "auto", label = "全部" }}]
"#
            )
        };
        let ok = body("[tools.io.workbench]\ntarget = \"sheet\"");
        let manifest = parse(&ok).expect("valid workbench");
        let io = manifest.tools[0].io.as_ref().expect("io present");
        assert_eq!(io.layout, IoLayout::Workbench);
        io.validate(manifest.api_version).expect("valid io");

        // 缺 [io.workbench]：列表点了也不知道回填谁。
        let no_section = body("");
        let error = parse(&no_section).unwrap_err();
        assert!(error.to_string().contains("workbench"), "{error}");

        // 缺 scan：列表没有来源。
        let no_scan = ok.replace("scan = { src = \"src_dir\" }", "");
        let error = parse(&no_scan).unwrap_err();
        assert!(error.to_string().contains("scan"), "{error}");

        // target 指向不存在的参数。
        let bad_target = body("[tools.io.workbench]\ntarget = \"elsewhere\"");
        let error = parse(&bad_target).unwrap_err();
        assert!(error.to_string().contains("target"), "{error}");

        // [io.workbench] 出现在别的布局下：无处安放。
        let plain = ok.replace("layout = \"workbench\"", "");
        let error = parse(&plain).unwrap_err();
        assert!(error.to_string().contains("workbench"), "{error}");
    }

    #[test]
    fn unknown_path_pick_is_rejected() {
        let source = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
allow_files = "read"
[[tools.io.params]]
id = "src"
kind = "path"
label = "源"
pick = "directory"
"#
        );
        assert!(parse(&source).is_err(), "typo in pick must not be ignored");
    }

    /// `pills` 与 `select` 同一套校验：必须有选项、default 必须命中。
    #[test]
    fn pills_follows_select_validation() {
        let body = |options: &str, default: &str| {
            format!(
                r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "version"
kind = "pills"
label = "系统版本"
{default}
{options}
"#
            )
        };
        let no_options = body("", "");
        let error = parse(&no_options).unwrap_err();
        assert!(
            error.to_string().contains("select requires options"),
            "{error}"
        );

        let ok = body(
            r#"options = [{ value = "all", label = "全部" }, { value = "3.0", label = "3.0" }]"#,
            r#"default = "all""#,
        );
        parse(&ok).expect("pills with valid options must load");

        let bad_default = body(
            r#"options = [{ value = "all", label = "全部" }]"#,
            r#"default = "zzz""#,
        );
        let error = parse(&bad_default).unwrap_err();
        assert!(
            error.to_string().contains("not one of the option values"),
            "{error}"
        );
    }

    /// 展示参数是只读的：必填与选项都在装载期拒绝。
    #[test]
    fn display_params_cannot_be_required_or_have_options() {
        let required = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "list"
kind = "display"
label = "列表"
required = true
"#
        );
        let error = parse(&required).unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error}");

        let with_options = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "list"
kind = "display"
label = "列表"
options = [{{ value = "a", label = "A" }}]
"#
        );
        let error = parse(&with_options).unwrap_err();
        assert!(error.to_string().contains("cannot have options"), "{error}");
    }

    /// 扫描声明：src 必须是已声明的 path 参数，且至少要有一个展示参数可回填。
    #[test]
    fn scan_declaration_is_validated() {
        let header = |scan: &str, extra: &str| {
            format!(
                r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
allow_files = "read"
{scan}
[[tools.io.params]]
id = "src_dir"
kind = "path"
label = "目录"
pick = "folder"
{extra}
"#
            )
        };
        let ok = header(
            r#"scan = { src = "src_dir" }"#,
            r#"
[[tools.io.params]]
id = "merge_versions"
kind = "display"
label = "待合并版本列表"
"#,
        );
        parse(&ok).expect("valid scan declaration must load");

        let unknown_src = header("scan = { src = \"elsewhere\" }", "");
        let error = parse(&unknown_src).unwrap_err();
        assert!(
            error.to_string().contains("not a declared param"),
            "{error}"
        );

        let not_path = header(
            r#"scan = { src = "src_dir" }"#,
            r#"
[[tools.io.params]]
id = "src_dir2"
kind = "text"
label = "目录"
"#,
        );
        // `src_dir` 是 path，但没有任何 display 参数可回填。
        let error = parse(&not_path).unwrap_err();
        assert!(error.to_string().contains("display param"), "{error}");
    }

    /// `layout = "job"` 能解析；老清单不带 layout 时缺省为通用页。
    #[test]
    fn layout_parses_and_defaults_to_default() {
        let job = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
allow_files = "read"
layout = "job"
run_label = "开始合并"
log_hint = "选择包目录后点击「开始合并」"
[[tools.io.params]]
id = "src_dir"
kind = "path"
label = "目录"
pick = "folder"
"#
        );
        let manifest = parse(&job).expect("valid manifest");
        let io = manifest.tools[0].io.as_ref().expect("io present");
        assert_eq!(io.layout, IoLayout::Job);
        assert_eq!(io.run_label, "开始合并");
        assert_eq!(io.log_hint, "选择包目录后点击「开始合并」");

        let plain = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
allow_files = "read"
[[tools.io.params]]
id = "src_dir"
kind = "path"
label = "目录"
pick = "folder"
"#
        );
        let manifest = parse(&plain).expect("valid manifest");
        let io = manifest.tools[0].io.as_ref().expect("io present");
        assert_eq!(io.layout, IoLayout::Default);
    }

    /// 组标题能解析、缺省为空（老清单零改动）。
    #[test]
    fn param_group_parses_and_defaults_to_empty() {
        let source = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "a"
kind = "switch"
label = "A"
group = "合并选项"
[[tools.io.params]]
id = "b"
kind = "switch"
label = "B"
"#
        );
        let manifest = parse(&source).expect("valid manifest");
        let io = manifest.tools[0].io.as_ref().expect("io present");
        assert_eq!(io.params[0].group, "合并选项");
        assert_eq!(io.params[1].group, "");
    }

    /// 条件显示的合法形态：引用**更早**声明的 select / switch，值是它的取值之一。
    #[test]
    fn visible_when_must_guard_an_earlier_enum_param() {
        let source = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "mode"
kind = "select"
label = "模式"
options = [{{ value = "a", label = "A" }}, {{ value = "b", label = "B" }}]
[[tools.io.params]]
id = "extra"
kind = "text"
label = "附加"
visible_when = {{ mode = "b" }}
"#
        );
        let manifest = parse(&source).expect("valid manifest");
        let io = manifest.tools[0].io.as_ref().expect("io present");
        io.validate(manifest.api_version).expect("valid io");
        let conditions = io.params[1].visible_when.as_ref().expect("parsed");
        assert_eq!(conditions.get("mode").map(String::as_str), Some("b"));
    }

    /// 三种错法都要在装载期报出来，不能运行期才发现「条件永远不成立」：
    /// 引用后面的参数（环形依赖）、引用非枚举参数、期望值不在选项里。
    #[test]
    fn invalid_visible_when_is_rejected_at_load_time() {
        let body = |guard: &str| {
            format!(
                r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "extra"
kind = "text"
label = "附加"
{guard}
[[tools.io.params]]
id = "mode"
kind = "select"
label = "模式"
options = [{{ value = "a", label = "A" }}]
[[tools.io.params]]
id = "note"
kind = "text"
label = "备注"
"#
            )
        };

        // ① 引用后面才声明的 `mode`：报错（顺带让 a→b→a 的环无解）。
        let forward = body(r#"visible_when = { mode = "a" }"#);
        let error = parse(&forward).expect_err("forward reference must fail");
        assert!(error.to_string().contains("declared before"), "{error}");

        // ② 期望值不在选项里：条件永远不成立，等于这个参数永远不显示。
        let bad_value = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "mode"
kind = "select"
label = "模式"
options = [{{ value = "a", label = "A" }}]
[[tools.io.params]]
id = "extra"
kind = "text"
label = "附加"
visible_when = {{ mode = "zzz" }}
"#
        );
        let manifest = parse(&bad_value).expect_err("unknown option value must fail");
        assert!(
            manifest
                .to_string()
                .contains("not one of its option values"),
            "{manifest}"
        );

        // ③ 守卫一个非枚举参数（text）：条件无从判断。
        let bad_guard = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "label"
kind = "text"
label = "标签"
[[tools.io.params]]
id = "extra"
kind = "text"
label = "附加"
visible_when = {{ label = "x" }}
"#
        );
        let manifest = parse(&bad_guard).expect_err("text guard must fail");
        assert!(
            manifest.to_string().contains("select or switch"),
            "{manifest}"
        );
    }

    #[test]
    fn tabs_style_requires_a_single_enum_param() {
        let ok = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "mode"
kind = "select"
label = "模式"
style = "tabs"
options = [{{ value = "a", label = "A" }}, {{ value = "b", label = "B" }}]
"#
        );
        let manifest = parse(&ok).expect("tabs select is valid");
        let io = manifest.tools[0].io.as_ref().expect("io present");
        assert_eq!(io.params[0].style, ParamStyle::Tabs);

        // 文本控件没有取值集合，画不出页签。
        let bad_kind = ok.replace("kind = \"select\"", "kind = \"text\"");
        let manifest = parse(&bad_kind).expect_err("tabs on text must fail");
        assert!(
            manifest.to_string().contains("requires a select or pills"),
            "{manifest}"
        );

        // 两行页签无处安放。
        let two_tabs = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
[[tools.io.params]]
id = "mode"
kind = "select"
label = "模式"
style = "tabs"
options = [{{ value = "a", label = "A" }}]
[[tools.io.params]]
id = "sub"
kind = "select"
label = "子模式"
style = "tabs"
options = [{{ value = "x", label = "X" }}]
"#
        );
        let manifest = parse(&two_tabs).expect_err("two tab params must fail");
        assert!(manifest.to_string().contains("at most one"), "{manifest}");
    }

    #[test]
    fn swap_fields_needs_two_row_fields() {
        let bad = format!(
            r#"{HEADER}
api_version = 2
[[tools]]
id = "dev.duck.sample.t"
name = "工具"
[tools.io]
swap_fields = true
[[tools.io.fields]]
id = "a"
kind = "textarea"
label = "A"
[[tools.io.fields]]
id = "b"
kind = "textarea"
label = "B"
"#
        );
        // 没声明并排 ⇒ 交换按钮无处可放。
        let manifest = parse(&bad).expect_err("swap without row must fail");
        assert!(manifest.to_string().contains("swap_fields"), "{manifest}");

        let ok = bad.replace(
            "[tools.io]\nswap_fields",
            "[tools.io]\nfields_layout = \"row\"\nswap_fields",
        );
        let manifest = parse(&ok).expect("row + swap is valid");
        assert_eq!(
            manifest.tools[0].io.as_ref().unwrap().fields_layout,
            FieldsLayout::Row
        );
    }
}
