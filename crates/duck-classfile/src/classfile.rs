//! Java `class` 文件解析：常量池 / 字段 / 方法 / 属性。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/decompiler.rs` 第 1–399 行
//! （数据结构 → `read_*` 读取器 → `parse_class_file`），一行未改。反编译主体与
//! `decompile_class_enhanced`（要 `ristretto_classfile`）没搬，理由见 crate 文档。
//!
//! 合并族只用得上 [`parse_class_file`] 返回的 `constant_pool`：`getVersion()` 方法
//! 在不在、常量池里有没有 `HUNDSUN_VERSION`、能不能正则抽到 `x.y.z`——这三问决定
//! 「同名 `.class` 该留谁」（见 `duck-classfile::version_extractor`）。

#![allow(clippy::needless_range_loop, clippy::manual_range_contains)]

// 与参考仓同一份 `use` 列表：`HashMap` / `Path` / `Read` / `ZipArchive` / `Serialize`
// 只有第 400 行之后（反编译、容器读取、序列化）才用到，随那部分一起搬来，这里先不引。

pub struct ClassFile {
    pub version: String,
    pub access_flags: u16,
    pub this_class: String,
    pub super_class: String,
    pub interfaces: Vec<String>,
    pub fields: Vec<FieldInfo>,
    pub methods: Vec<MethodInfo>,
    pub attributes: Vec<AttributeInfo>,
    pub constant_pool: ConstantPool,
}

#[derive(Clone)]
pub struct FieldInfo {
    pub access_flags: u16,
    pub name: String,
    pub descriptor: String,
    pub attributes: Vec<AttributeInfo>,
}

#[derive(Clone)]
pub struct MethodInfo {
    pub access_flags: u16,
    pub name: String,
    pub descriptor: String,
    pub attributes: Vec<AttributeInfo>,
    pub code: Option<CodeAttribute>,
}

#[derive(Clone)]
pub struct CodeAttribute {
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: Vec<u8>,
    pub exception_table: Vec<ExceptionEntry>,
    pub attributes: Vec<AttributeInfo>,
}

#[derive(Clone)]
pub struct ExceptionEntry {
    pub start_pc: u16,
    pub end_pc: u16,
    pub handler_pc: u16,
    pub catch_type: Option<String>,
}

#[derive(Clone)]
pub struct AttributeInfo {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum CpEntry {
    Utf8(String),
    Integer(i32),
    Float(f32),
    Long(i64),
    Double(f64),
    Class(u16),
    String(u16),
    FieldRef { class_index: u16, name_and_type_index: u16 },
    MethodRef { class_index: u16, name_and_type_index: u16 },
    InterfaceMethodRef { class_index: u16, name_and_type_index: u16 },
    NameAndType { name_index: u16, descriptor_index: u16 },
    MethodHandle { reference_kind: u8, reference_index: u16 },
    MethodType(u16),
    Dynamic { bootstrap_method_attr_index: u16, name_and_type_index: u16 },
    InvokeDynamic { bootstrap_method_attr_index: u16, name_and_type_index: u16 },
    Module(u16),
    Package(u16),
    Unknown,
}

#[derive(Clone)]
pub struct ConstantPool {
    pub entries: Vec<CpEntry>,
}

impl ConstantPool {
    pub fn new_empty() -> Self {
        ConstantPool { entries: vec![CpEntry::Unknown] }
    }
    fn new() -> Self {
        ConstantPool { entries: vec![CpEntry::Unknown] }
    }

    fn get_utf8(&self, index: u16) -> String {
        let idx = index as usize;
        if idx < self.entries.len() {
            match &self.entries[idx] {
                CpEntry::Utf8(s) => s.clone(),
                _ => format!("#{}", index),
            }
        } else {
            format!("#{}", index)
        }
    }

    fn get_class_name(&self, index: u16) -> String {
        let idx = index as usize;
        if idx < self.entries.len() {
            match &self.entries[idx] {
                CpEntry::Class(name_index) => {
                    self.get_utf8(*name_index)
                }
                _ => format!("#{}", index),
            }
        } else {
            format!("#{}", index)
        }
    }

    fn get_constant_value(&self, index: u16) -> Option<String> {
        let idx = index as usize;
        if idx >= self.entries.len() { return None; }
        match &self.entries[idx] {
            CpEntry::Integer(v) => Some(v.to_string()),
            CpEntry::Float(v) => {
                if v.is_nan() { Some("0.0f / 0.0f".to_string()) }
                else if v.is_infinite() { Some(if *v > 0.0 { "1.0f / 0.0f".to_string() } else { "-1.0f / 0.0f".to_string() }) }
                else { Some(format!("{}f", v)) }
            }
            CpEntry::Long(v) => Some(format!("{}L", v)),
            CpEntry::Double(v) => {
                if v.is_nan() { Some("0.0 / 0.0".to_string()) }
                else if v.is_infinite() { Some(if *v > 0.0 { "1.0 / 0.0".to_string() } else { "-1.0 / 0.0".to_string() }) }
                else { Some(v.to_string()) }
            }
            CpEntry::String(si) => {
                let s = self.get_utf8(*si);
                Some(format!("\"{}\"", s.escape_default()))
            }
            _ => None,
        }
    }

    fn get_name_and_type(&self, index: u16) -> (String, String) {
        let idx = index as usize;
        if idx < self.entries.len() {
            match &self.entries[idx] {
                CpEntry::NameAndType { name_index, descriptor_index } => {
                    (self.get_utf8(*name_index), self.get_utf8(*descriptor_index))
                }
                _ => (format!("#{}", index), String::new()),
            }
        } else {
            (format!("#{}", index), String::new())
        }
    }

    fn get_method_ref(&self, index: u16) -> (String, String, String) {
        let idx = index as usize;
        if idx < self.entries.len() {
            match &self.entries[idx] {
                CpEntry::MethodRef { class_index, name_and_type_index }
                | CpEntry::InterfaceMethodRef { class_index, name_and_type_index } => {
                    let class_name = self.get_class_name(*class_index);
                    let (name, desc) = self.get_name_and_type(*name_and_type_index);
                    (class_name, name, desc)
                }
                _ => (format!("#{}", index), String::new(), String::new()),
            }
        } else {
            (format!("#{}", index), String::new(), String::new())
        }
    }

    fn get_field_ref(&self, index: u16) -> (String, String, String) {
        let idx = index as usize;
        if idx < self.entries.len() {
            match &self.entries[idx] {
                CpEntry::FieldRef { class_index, name_and_type_index } => {
                    let class_name = self.get_class_name(*class_index);
                    let (name, desc) = self.get_name_and_type(*name_and_type_index);
                    (class_name, name, desc)
                }
                _ => (format!("#{}", index), String::new(), String::new()),
            }
        } else {
            (format!("#{}", index), String::new(), String::new())
        }
    }
}

const MAGIC: u32 = 0xCAFEBABE;

// 底层读取器**永不 panic**：读取越过数据末尾时返回 0 / 截断后的剩余字节。
//
// 需求单合并会对同名候选 Class 调用版本提取，若包里混入被截断或损坏的
// `.class`，越界 panic 会直接中断整个合并流程；改为越界即回退为「读不到」，
// 由上层据 magic / 版本号判定该文件无效（返回 -1 → 回退按修改日期裁决）。
fn read_u1(data: &[u8], pos: &mut usize) -> u8 {
    let v = if *pos < data.len() { data[*pos] } else { 0 };
    *pos = pos.saturating_add(1);
    v
}

fn read_u2(data: &[u8], pos: &mut usize) -> u16 {
    u16::from_be_bytes([read_u1(data, pos), read_u1(data, pos)])
}

fn read_u4(data: &[u8], pos: &mut usize) -> u32 {
    u32::from_be_bytes([
        read_u1(data, pos),
        read_u1(data, pos),
        read_u1(data, pos),
        read_u1(data, pos),
    ])
}

fn read_bytes(data: &[u8], pos: &mut usize, len: usize) -> Vec<u8> {
    let start = (*pos).min(data.len());
    let end = pos.saturating_add(len).min(data.len());
    let v = data[start..end].to_vec();
    *pos = pos.saturating_add(len);
    v
}

fn read_cp_entry(data: &[u8], pos: &mut usize) -> CpEntry {
    let tag = read_u1(data, pos);
    match tag {
        1 => {
            let len = read_u2(data, pos) as usize;
            let bytes = read_bytes(data, pos, len);
            CpEntry::Utf8(String::from_utf8_lossy(&bytes).to_string())
        }
        3 => CpEntry::Integer(read_u4(data, pos) as i32),
        4 => CpEntry::Float(f32::from_bits(read_u4(data, pos))),
        5 => {
            let high = read_u4(data, pos);
            let low = read_u4(data, pos);
            CpEntry::Long(((high as i64) << 32) | (low as i64))
        }
        6 => {
            let high = read_u4(data, pos);
            let low = read_u4(data, pos);
            CpEntry::Double(f64::from_bits(((high as u64) << 32) | (low as u64)))
        }
        7 => CpEntry::Class(read_u2(data, pos)),
        8 => CpEntry::String(read_u2(data, pos)),
        9 => CpEntry::FieldRef { class_index: read_u2(data, pos), name_and_type_index: read_u2(data, pos) },
        10 => CpEntry::MethodRef { class_index: read_u2(data, pos), name_and_type_index: read_u2(data, pos) },
        11 => CpEntry::InterfaceMethodRef { class_index: read_u2(data, pos), name_and_type_index: read_u2(data, pos) },
        12 => CpEntry::NameAndType { name_index: read_u2(data, pos), descriptor_index: read_u2(data, pos) },
        15 => {
            let kind = read_u1(data, pos);
            let idx = read_u2(data, pos);
            CpEntry::MethodHandle { reference_kind: kind, reference_index: idx }
        }
        16 => CpEntry::MethodType(read_u2(data, pos)),
        17 => CpEntry::Dynamic { bootstrap_method_attr_index: read_u2(data, pos), name_and_type_index: read_u2(data, pos) },
        18 => CpEntry::InvokeDynamic { bootstrap_method_attr_index: read_u2(data, pos), name_and_type_index: read_u2(data, pos) },
        19 => CpEntry::Module(read_u2(data, pos)),
        20 => CpEntry::Package(read_u2(data, pos)),
        _ => CpEntry::Unknown,
    }
}

fn read_attribute_info(data: &[u8], pos: &mut usize, cp: &ConstantPool) -> AttributeInfo {
    let name_index = read_u2(data, pos);
    let length = read_u4(data, pos) as usize;
    let attr_data = read_bytes(data, pos, length);
    AttributeInfo {
        name: cp.get_utf8(name_index),
        data: attr_data,
    }
}

fn read_code_attribute(data: &[u8], cp: &ConstantPool) -> Option<CodeAttribute> {
    let mut pos = 0usize;
    let max_stack = read_u2(data, &mut pos);
    let max_locals = read_u2(data, &mut pos);
    let code_length = read_u4(data, &mut pos) as usize;
    let code = read_bytes(data, &mut pos, code_length);

    let table_length = read_u2(data, &mut pos);
    let mut exception_table = Vec::new();
    for _ in 0..table_length {
        let start_pc = read_u2(data, &mut pos);
        let end_pc = read_u2(data, &mut pos);
        let handler_pc = read_u2(data, &mut pos);
        let catch_type_index = read_u2(data, &mut pos);
        let catch_type = if catch_type_index == 0 {
            None
        } else {
            Some(cp.get_class_name(catch_type_index).replace('/', "."))
        };
        exception_table.push(ExceptionEntry { start_pc, end_pc, handler_pc, catch_type });
    }

    let attrs_count = read_u2(data, &mut pos);
    let mut attributes = Vec::new();
    for _ in 0..attrs_count {
        attributes.push(read_attribute_info(data, &mut pos, cp));
    }

    Some(CodeAttribute { max_stack, max_locals, code, exception_table, attributes })
}

pub fn parse_class_file(data: &[u8]) -> Result<ClassFile, String> {
    let mut pos = 0usize;

    let magic = read_u4(data, &mut pos);
    if magic != MAGIC {
        return Err(format!("无效的 class 文件: magic=0x{:08X}", magic));
    }

    let minor = read_u2(data, &mut pos);
    let major = read_u2(data, &mut pos);
    let version = format!("{}.{}", major, minor);

    let cp_count = read_u2(data, &mut pos) as usize;
    let mut cp = ConstantPool::new();
    let mut i = 1;
    while i < cp_count {
        let entry = read_cp_entry(data, &mut pos);
        let is_wide = matches!(entry, CpEntry::Long(_) | CpEntry::Double(_));
        cp.entries.push(entry);
        i += 1;
        if is_wide && i < cp_count {
            cp.entries.push(CpEntry::Unknown);
            i += 1;
        }
    }

    let access_flags = read_u2(data, &mut pos);
    let this_class = read_u2(data, &mut pos);
    let super_class = read_u2(data, &mut pos);

    let iface_count = read_u2(data, &mut pos);
    let mut interfaces = Vec::new();
    for _ in 0..iface_count {
        interfaces.push(cp.get_class_name(read_u2(data, &mut pos)).replace('/', "."));
    }

    let field_count = read_u2(data, &mut pos);
    let mut fields = Vec::new();
    for _ in 0..field_count {
        let f_access = read_u2(data, &mut pos);
        let f_name = cp.get_utf8(read_u2(data, &mut pos));
        let f_desc = cp.get_utf8(read_u2(data, &mut pos));
        let f_attr_count = read_u2(data, &mut pos);
        let mut f_attrs = Vec::new();
        for _ in 0..f_attr_count {
            f_attrs.push(read_attribute_info(data, &mut pos, &cp));
        }
        fields.push(FieldInfo { access_flags: f_access, name: f_name, descriptor: f_desc, attributes: f_attrs });
    }

    let method_count = read_u2(data, &mut pos);
    let mut methods = Vec::new();
    for _ in 0..method_count {
        let m_access = read_u2(data, &mut pos);
        let m_name = cp.get_utf8(read_u2(data, &mut pos));
        let m_desc = cp.get_utf8(read_u2(data, &mut pos));
        let m_attr_count = read_u2(data, &mut pos);
        let mut m_attrs = Vec::new();
        let mut code_attr = None;
        for _ in 0..m_attr_count {
            let attr = read_attribute_info(data, &mut pos, &cp);
            if attr.name == "Code" {
                code_attr = read_code_attribute(&attr.data, &cp);
            }
            m_attrs.push(attr);
        }
        methods.push(MethodInfo {
            access_flags: m_access,
            name: m_name,
            descriptor: m_desc,
            attributes: m_attrs,
            code: code_attr,
        });
    }

    let attr_count = read_u2(data, &mut pos);
    let mut attributes = Vec::new();
    for _ in 0..attr_count {
        attributes.push(read_attribute_info(data, &mut pos, &cp));
    }

    Ok(ClassFile {
        version,
        access_flags,
        this_class: cp.get_class_name(this_class).replace('/', "."),
        super_class: cp.get_class_name(super_class).replace('/', "."),
        interfaces,
        fields,
        methods,
        attributes,
        constant_pool: cp,
    })
}
