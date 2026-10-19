// Duck Toolbox 插件：SQL 改写（duck.sql.sql-rewrite）。
//
// 契约：`handle(toolId, input)` 返回字符串结果；参数经 `duck.params` 注入。
//
// **来源与搬运原则**：参考仓 `ifm_ai_tools` 的这两个功能**没有 Rust 后端**
// （处理逻辑全在前端页面里），属于「纯前端算法」，按移植规矩允许直接搬前端逻辑。
// 下面每个函数都从源页面整段照抄，只把 Vue 的 `ref` / 模板接线换成 `duck.params`：
//   - `src/tools/SqlReplace.vue`（365 行）→ 模式 `param`
//   - `src/tools/SqlReExec.vue`（1,093 行）→ 模式 `reexec` / `clean` / `backup`
// 算法一行没重写：重写等于要重新验证四套 SQL 语法的边界，正是移植要避免的成本。
//
// 与源页面的差异只有三处，全是**接线**：
//   1. `ref` 变成参数（`commentDelete` → `comment_delete`，等）；
//   2. 源页面有状态栏，这里没有 ⇒ 统计走 `duck.notify`，返回值保持**纯 SQL**
//      （结果区要能直接全选复制去执行，前面挂一行统计就毁了）；
//   3. 源页面里 `generateCreateTableCheck` / `generateAlterTableCheck` /
//      `generateDropTableCheck` / `generateCreateIndexCheck` / `isDeleteForTable`
//      是**从未被引用的死代码**（全文各出现 1 次，只有定义），不移植。

const DEFAULTS = {
  mode: "param",
  schema: "",
  add_select_check: "true",
  add_delete: "false",
  comment_delete: "false",
  db_type: "oracle",
};

function takeParams() {
  const declared = (typeof duck.params === "object" && duck.params) || {};
  return { ...DEFAULTS, ...declared };
}

/// 开关值可能是布尔也可能是字符串（清单 `default` 是字符串形态），统一收口。
function flag(value, fallback) {
  if (value === undefined || value === null || value === "") return fallback;
  return value === true || value === "true";
}

// ════════════════════════════════════════════════════════════════════════
// 一、参数替换：应用日志里的含参 SQL → 可执行 SQL（SqlReplace.vue）
// ════════════════════════════════════════════════════════════════════════

function extractPlaceholders(sql) {
  const positions = [];
  let inQuote = null;
  let i = 0;
  while (i < sql.length) {
    const c = sql[i];
    if (inQuote) {
      if (c === "\\") { i += 2; continue; }
      if (c === inQuote) inQuote = null;
      i++;
      continue;
    }
    if (c === "'" || c === '"') { inQuote = c; i++; continue; }
    if (c === "?") positions.push(i);
    i++;
  }
  return positions;
}

function wrapStringParam(param) {
  const trimmed = param.trim();
  if (trimmed === "") return "''";
  if (/^-?\d+(\.\d+)?$/.test(trimmed)) return trimmed;
  if ((trimmed.startsWith("'") && trimmed.endsWith("'"))
    || (trimmed.startsWith('"') && trimmed.endsWith('"'))) return trimmed;
  const escaped = trimmed.replace(/'/g, "''");
  return `'${escaped}'`;
}

function parseParams(text) {
  const params = [];
  let current = "";
  let inQuote = null;
  let i = 0;

  while (i < text.length) {
    const c = text[i];
    if (inQuote) {
      if (c === "\\") { current += c + (text[i + 1] || ""); i += 2; continue; }
      current += c;
      if (c === inQuote) inQuote = null;
      i++;
      continue;
    }
    if (c === "'" || c === '"') {
      inQuote = c;
      current += c;
      i++;
      continue;
    }
    if (c === "," || c === "，") {
      const trimmed = current.trim();
      if (trimmed) params.push(trimmed);
      current = "";
      i++;
      continue;
    }
    if (!/\s/.test(c) || current.length > 0) {
      current += c;
    }
    i++;
  }
  const trimmed = current.trim();
  if (trimmed) params.push(trimmed);
  return params;
}

function addSchemaPrefix(sql, s) {
  if (!s.trim()) return sql;
  const schemaName = s.trim();
  const keywords = /\b(FROM|INTO|UPDATE|JOIN|LEFT\s+JOIN|RIGHT\s+JOIN|INNER\s+JOIN|OUTER\s+JOIN|CROSS\s+JOIN|DELETE\s+FROM)\b/gi;
  let result = "";
  let lastIndex = 0;
  let match;
  while ((match = keywords.exec(sql)) !== null) {
    result += sql.slice(lastIndex, match.index + match[0].length);
    const afterKeyword = sql.slice(match.index + match[0].length);
    const tableMatch = afterKeyword.match(/^\s+([`"]?\w+[`"]?)(\s*\.)/);
    if (tableMatch) {
      const dotPos = match.index + match[0].length + tableMatch[0].indexOf(".");
      result += sql.slice(match.index + match[0].length, dotPos);
    } else {
      const tableMatch2 = afterKeyword.match(/^\s+([`"]?\w+[`"]?)/);
      if (tableMatch2 && tableMatch2.index !== undefined) {
        const tableName = tableMatch2[1].replace(/[`"]/g, "");
        result += ` ${schemaName}.${tableName}`;
        lastIndex = match.index + match[0].length + tableMatch2.index + tableMatch2[0].length;
        continue;
      }
    }
    lastIndex = match.index + match[0].length;
  }
  result += sql.slice(lastIndex);
  return result;
}

function replaceSql(sql, params, s) {
  const processedSql = addSchemaPrefix(sql, s);
  const positions = extractPlaceholders(processedSql);
  if (!positions.length) return processedSql;
  const chars = processedSql.split("");
  for (let i = 0; i < Math.min(positions.length, params.length); i++) {
    chars[positions[i]] = wrapStringParam(params[i]);
  }
  return chars.join("");
}

function extractSqlAndParams(line) {
  line = line.replace(/\uff1f/g, "?");

  const lastQ = line.lastIndexOf("?");
  if (lastQ < 0) return null;

  const afterLastQ = line.slice(lastQ + 1);

  const paramPatterns = [
    /请求参数[:：]\s*(.+)/,
    /当前参数[:：]\s*(.+)/,
    /参数[:：]\s*(.+)/,
    /【\s*参数\s*】\s*(.+)/,
    /\[\s*参数\s*\]\s*(.+)/,
    /[pP]arams?[:：]\s*(.+)/,
    /[pP]arameters?[:：]\s*(.+)/,
    /[aA]rgs?[:：]\s*(.+)/,
  ];

  let paramsStr = null;
  let sqlEndOffset = lastQ + 1;

  for (const pat of paramPatterns) {
    const m = afterLastQ.match(pat);
    if (m) {
      paramsStr = m[1].trim();
      sqlEndOffset = lastQ + 1 + m.index;
      break;
    }
  }

  if (!paramsStr) {
    const remainder = afterLastQ.trim();
    if (remainder) {
      paramsStr = remainder;
    } else {
      return null;
    }
  }

  let sql = line.slice(0, sqlEndOffset).trim();
  sql = sql.replace(/^(?:SQL执行,\s*)?(?:当前执行[:：]\s*)?(?:sql|SQL|当前SQL)[:：]\s*/i, "").trim();

  const placeholders = extractPlaceholders(sql);
  if (!placeholders.length) return null;

  const params = parseParams(paramsStr);
  if (params.length !== placeholders.length) return null;

  return { sql, params };
}

function replaceLog(text, schema) {
  const lines = text.split("\n").filter((l) => l.trim());
  const outputLines = [];
  let replacedCount = 0;
  let failCount = 0;
  let mismatchLines = 0;

  for (const line of lines) {
    const parsed = extractSqlAndParams(line);
    if (parsed) {
      outputLines.push(replaceSql(parsed.sql, parsed.params, schema) + ";");
      outputLines.push("");
      replacedCount++;
    } else {
      if (line.includes("?") && /(?:sql|参数)/i.test(line)) mismatchLines++;
      outputLines.push(line);
      failCount++;
    }
  }

  return { text: outputLines.join("\n").trim(), replacedCount, failCount, mismatchLines };
}

// ════════════════════════════════════════════════════════════════════════
// 二、可重复执行 / 纯净版 / 备份（SqlReExec.vue）
// ════════════════════════════════════════════════════════════════════════

// 已知表的业务主键：给 INSERT 生成「先查一下」与「先删一下」用的 WHERE。
const KNOWN_TABLE_KEYS = {
  tbparam: ["ta_code", "param_id"],
  TBPARAM: ["ta_code", "param_id"],
  tbtrans: ["trans_code"],
  TBTRANS: ["trans_code"],
  tbtaskpool: ["market_code", "task_code"],
  TBTASKPOOL: ["market_code", "task_code"],
  tsys_menu: ["menu_code"],
  TSYS_MENU: ["menu_code"],
  tsys_dict_item: ["dict_entry_code", "dict_item_code"],
  tsys_trans: ["trans_code"],
  tsys_subtrans: ["trans_code", "sub_trans_code"],
  tbdataelement: ["id"],
  tbprdparameter: ["table_name", "field_code"],
  tbpageelement: ["id"],
  tbsysversion: ["version_no"],
  tbprdparamvalue: ["table_name", "prd_code", "field_code"],
};

function parseInsert(sql) {
  const s = sql.replace(/\s+/g, " ").trim();
  const tableM = s.match(/INSERT\s+INTO\s+(\w+)/i);
  if (!tableM) return null;
  const table = tableM[1];

  let i = tableM[0].length;
  let columns = [];
  let inStr = false;

  while (i < s.length && s[i] === " ") i++;
  if (i < s.length && s[i] === "(") {
    i++;
    let colBuf = "";
    inStr = false;
    while (i < s.length) {
      const ch = s[i];
      if (ch === "'") inStr = !inStr;
      else if (ch === ")" && !inStr) break;
      colBuf += ch;
      i++;
    }
    i++;
    columns = colBuf.split(",").map((c) => c.trim()).filter(Boolean);
  }

  while (i < s.length && s[i] === " ") i++;
  const valuesLabel = s.substring(i).match(/^VALUES\s*/i);
  if (!valuesLabel) return null;
  i += valuesLabel[0].length;

  while (i < s.length && s[i] === " ") i++;
  if (i >= s.length || s[i] !== "(") return null;
  i++;
  let valBuf = "";
  inStr = false;
  let depth = 0;
  while (i < s.length) {
    const ch = s[i];
    if (ch === "'") {
      if (inStr && i + 1 < s.length && s[i + 1] === "'") {
        valBuf += "''";
        i += 2;
        continue;
      }
      inStr = !inStr;
      valBuf += ch;
    } else if (ch === "(" && !inStr) {
      depth++;
      if (depth > 0) valBuf += ch;
    } else if (ch === ")" && !inStr) {
      if (depth === 0) break;
      depth--;
      valBuf += ch;
    } else {
      valBuf += ch;
    }
    i++;
  }

  const values = parseValues(valBuf);
  return { table, columns, values };
}

function parseValues(valStr) {
  const values = [];
  let current = "";
  let inStr = false;
  let i = 0;
  while (i < valStr.length) {
    const ch = valStr[i];
    if (ch === "'") {
      if (inStr && i + 1 < valStr.length && valStr[i + 1] === "'") {
        current += "''";
        i += 2;
        continue;
      }
      inStr = !inStr;
      current += ch;
    } else if (ch === "," && !inStr) {
      values.push(current.trim());
      current = "";
    } else {
      current += ch;
    }
    i++;
  }
  if (current.trim()) values.push(current.trim());
  return values;
}

function extractKeyWhere(table, columns, values) {
  const lower = table.toLowerCase();
  const keys = KNOWN_TABLE_KEYS[lower] || KNOWN_TABLE_KEYS[table];
  if (keys) {
    const parts = [];
    if (columns.length > 0) {
      for (const key of keys) {
        const idx = columns.findIndex((c) => c.toLowerCase() === key.toLowerCase());
        if (idx >= 0 && idx < values.length && values[idx]) {
          parts.push(`${key}=${values[idx]}`);
        }
      }
    } else {
      for (let i = 0; i < keys.length && i < values.length; i++) {
        parts.push(`${keys[i]}=${values[i]}`);
      }
    }
    if (parts.length > 0) return parts.join(" and ");
  }

  if (values.length > 0 && columns.length > 0) {
    const parts = [];
    const count = Math.min(2, columns.length, values.length);
    for (let i = 0; i < count; i++) {
      if (columns[i] && values[i]) {
        parts.push(`${columns[i]}=${values[i]}`);
      }
    }
    if (parts.length > 0) return parts.join(" and ");
  }
  return null;
}

function generateSelectCheck(insertSql) {
  const parsed = parseInsert(insertSql);
  if (!parsed) return null;
  const where = extractKeyWhere(parsed.table, parsed.columns, parsed.values);
  if (!where) return null;
  return `select * from ${parsed.table} where ${where};`;
}

function generateDelete(insertSql) {
  const parsed = parseInsert(insertSql);
  if (!parsed) return null;
  const where = extractKeyWhere(parsed.table, parsed.columns, parsed.values);
  if (!where) return null;
  return `delete from ${parsed.table} where ${where};`;
}

function getTableName(sql) {
  const patterns = [
    /INSERT\s+INTO\s+(\w+)/i,
    /CREATE\s+TABLE\s+(\w+)/i,
    /DROP\s+TABLE\s+(\w+)/i,
    /ALTER\s+TABLE\s+(\w+)/i,
    /DELETE\s+FROM\s+(\w+)/i,
    /TRUNCATE\s+TABLE\s+(\w+)/i,
    /UPDATE\s+(\w+)/i,
  ];
  for (const p of patterns) {
    const m = sql.match(p);
    if (m) return m[1];
  }
  return null;
}

function splitStatements(text) {
  const lines = text.split("\n");
  const stmts = [];
  let buf = [];

  for (const line of lines) {
    const trimmed = line.trim();

    const isComment = trimmed.startsWith("--") || trimmed.startsWith("/*") || trimmed.startsWith("*");
    if (isComment || trimmed.endsWith("*/") || trimmed === "*/") {
      if (buf.length > 0) {
        const hasContent = buf.some((l) => l.trim().length > 0 && !l.trim().startsWith("--"));
        if (hasContent) {
          buf.push(line);
        } else {
          buf = [];
          stmts.push(line);
        }
      } else {
        stmts.push(line);
      }
      continue;
    }

    buf.push(line);

    const upper = trimmed.toUpperCase();
    if (upper === "/" || trimmed.endsWith("/")) {
      if (buf.length > 1) {
        buf.pop();
        if (buf.length > 0) {
          const joined = buf.join("\n").replace(/^\n+/, "");
          if (joined.trim()) stmts.push(joined);
        }
        stmts.push(line);
        buf = [];
      } else {
        stmts.push(line);
        buf = [];
      }
      continue;
    }

    if (trimmed.endsWith(";")) {
      const joined = buf.join("\n").replace(/^\n+/, "").replace(/\n+$/, "");
      if (joined.trim()) stmts.push(joined);
      buf = [];
    }
  }

  if (buf.length > 0) stmts.push(buf.join("\n"));
  return stmts;
}

function processContent(text, options) {
  const stmts = splitStatements(text);
  const result = [];
  let savedDel = null;
  let savedTable = null;

  function flushSaved() {
    if (savedDel) { result.push(savedDel); savedDel = null; savedTable = null; }
  }

  for (let i = 0; i < stmts.length; i++) {
    const stmt = stmts[i];
    const t = stmt.trim();

    if (!t || t.startsWith("/*") || t.startsWith("*") || t.startsWith("/") || t.endsWith("*/")) {
      flushSaved();
      result.push(t);
      continue;
    }
    if (t.startsWith("--")) {
      flushSaved();
      result.push("/* " + t.replace(/^--\s*/, "").replace(/;\s*$/, "") + " */");
      continue;
    }

    const up = t.toUpperCase();

    if (up.startsWith("DELETE FROM")) {
      const table = getTableName(t);
      if (table) {
        for (let j = i + 1; j < stmts.length; j++) {
          const n = stmts[j].trim();
          if (!n || n.startsWith("--") || n.startsWith("/*") || n.startsWith("*") || n.endsWith("*/")) continue;
          const isIns = n.toUpperCase().startsWith("INSERT INTO")
            && getTableName(n)?.toLowerCase() === table.toLowerCase();
          if (isIns) {
            flushSaved();
            savedDel = options.commentDelete ? "/* " + t.replace(/;\s*$/, "") + " */" : t;
            savedTable = table.toLowerCase();
          } else {
            flushSaved();
            result.push(t);
          }
          break;
        }
      } else {
        flushSaved();
        result.push(t);
      }
      continue;
    }

    if (up.startsWith("INSERT INTO")) {
      const table = getTableName(t);
      const isSelect = up.includes("SELECT");
      let check = null;
      let genDel = null;

      if (options.addSelectCheck && !isSelect) {
        check = generateSelectCheck(t);
      }
      if (options.addDelete && !isSelect) {
        genDel = generateDelete(t);
        if (genDel && savedTable === table?.toLowerCase()) genDel = null;
      }

      if (check) {
        if (result.length > 0) {
          const last = result[result.length - 1].trim();
          if (last.endsWith(";")) result.push("");
        }
        result.push("/* " + check + " */");
      }

      if (savedDel && savedTable === table?.toLowerCase()) {
        result.push(savedDel);
        savedDel = null;
        savedTable = null;
      }
      if (genDel) result.push("/* " + genDel + " */");
      result.push(t);
      flushSaved();
      continue;
    }

    if (up.startsWith("UPDATE")) {
      if (options.addSelectCheck) {
        const tbl = getTableName(t);
        const wm = t.match(/WHERE\s+(.+)/i);
        if (tbl && wm) {
          if (result.length > 0) {
            const last = result[result.length - 1].trim();
            if (last.endsWith(";")) result.push("");
          }
          result.push(`/* select * from ${tbl} where ${wm[1].replace(/;\s*$/, "")}; */`);
        }
      }
      flushSaved();
      result.push(t);
      continue;
    }

    flushSaved();
    result.push(t);
  }

  flushSaved();
  const joined = result.join("\n").replace(/\n{3,}/g, "\n\n");
  return joined.replace(/;\n(\/\*)/g, ";\n\n$1");
}

function cleanSql(text) {
  const lines = text.split("\n");
  const result = [];
  let inBlock = false;

  function findInlineComment(line) {
    let inStr = false;
    for (let i = 0; i < line.length; i++) {
      if (line[i] === "'") inStr = !inStr;
      else if (line[i] === "-" && i + 1 < line.length && line[i + 1] === "-" && !inStr) return i;
    }
    return -1;
  }

  for (const rawLine of lines) {
    const s = rawLine.trimStart();

    if (inBlock) {
      if (s.includes("*/")) inBlock = false;
      continue;
    }

    if (s.startsWith("/*")) {
      const ei = s.indexOf("*/", 2);
      if (ei === -1) inBlock = true;
      continue;
    }

    if (s.startsWith("--")) {
      continue;
    }

    const ici = findInlineComment(rawLine);
    if (ici >= 0) {
      result.push(rawLine.substring(0, ici).trimEnd());
    } else {
      result.push(rawLine);
    }
  }

  const stmts = splitStatements(result.join("\n"));
  return stmts.filter((s) => s.trim()).map((s) => s.trim()).join("\n\n");
}

function generateBackupSql(text, dbType) {
  const stmts = splitStatements(text);
  const today = new Date().toISOString().slice(0, 10).replace(/-/g, "");
  const currentDbType = dbType;

  const backupTables = [];
  const processedTables = new Set();
  // DROP 特殊处理：被 DROP 且脚本中未重建的表，回滚时从备份表重建；
  // DROP 后又 CREATE 的仍走 DELETE + INSERT。
  const droppedTables = new Set();
  const recreatedTables = new Set();

  const backupPatterns = [
    /INSERT\s+INTO/i,
    /UPDATE\s/i,
    /DELETE\s+FROM/i,
    /DROP\s+TABLE/i,
    /ALTER\s+TABLE/i,
    /TRUNCATE\s+TABLE/i,
    /DROP\s+INDEX/i,
    /DROP\s+VIEW/i,
    /DROP\s+PROCEDURE/i,
    /DROP\s+FUNCTION/i,
  ];

  for (const stmt of stmts) {
    const t = stmt.trim();
    if (!t || t.startsWith("--") || t.startsWith("/*") || t.startsWith("*") || t.endsWith("*/") || t === "/") continue;

    const up = t.toUpperCase();
    const table = getTableName(t);
    if (!table) continue;

    const tableKey = table.toLowerCase();
    if (/DROP\s+TABLE/i.test(up)) {
      droppedTables.add(tableKey);
    } else if (/CREATE\s+TABLE/i.test(up) && droppedTables.has(tableKey)) {
      recreatedTables.add(tableKey);
    }

    if (backupPatterns.some((p) => p.test(up)) && !processedTables.has(tableKey)) {
      processedTables.add(tableKey);
      backupTables.push(table);
    }
  }

  if (backupTables.length === 0) return "";

  function generateCreateTableAs(target, source) {
    if (currentDbType === "db2") {
      return `CREATE TABLE ${target} AS (SELECT * FROM ${source}) WITH DATA`;
    }
    return `CREATE TABLE ${target} AS SELECT * FROM ${source}`;
  }

  const rule = "-- ========================================";
  const lines = [];
  lines.push(rule);
  lines.push(duck.t("-- 数据库类型: {}", currentDbType.toUpperCase()));
  lines.push(duck.t("-- 备份方式：全表备份（CREATE TABLE 表_yyyymmdd AS SELECT * FROM 表）"));
  lines.push(duck.t("-- 回滚默认使用 delete，大表建议手动改为 truncate 以提升性能"));
  lines.push(duck.t("-- 被 DROP 且脚本中未重建的表，回滚为从备份表重建（CREATE TABLE ... AS SELECT * FROM 备份表）"));
  lines.push(rule);
  lines.push("");
  lines.push(duck.t("-- 备份表清单"));
  lines.push(rule);
  for (const table of backupTables) {
    lines.push(duck.t("-- {}（全表备份）", table));
  }
  lines.push("");
  lines.push(rule);
  lines.push(duck.t("-- 备份 SQL（先执行）"));
  lines.push(rule);
  for (const table of backupTables) {
    lines.push(generateCreateTableAs(`${table}_${today}`, table) + ";");
  }
  lines.push("");
  lines.push(rule);
  lines.push(duck.t("-- 回滚 SQL（如需恢复数据时执行）"));
  lines.push(rule);
  for (const table of backupTables) {
    const backupTable = `${table}_${today}`;
    if (droppedTables.has(table.toLowerCase()) && !recreatedTables.has(table.toLowerCase())) {
      lines.push(duck.t("-- {} 已被 DROP，回滚时从备份表重建", table));
      lines.push(generateCreateTableAs(table, backupTable) + ";");
    } else {
      lines.push(`DELETE FROM ${table};`);
      lines.push(`INSERT INTO ${table} SELECT * FROM ${backupTable};`);
    }
  }

  return lines.join("\n");
}

// ════════════════════════════════════════════════════════════════════════
// 三、模式分派
// ════════════════════════════════════════════════════════════════════════

/// 源页面状态栏里的「共 N 条语句」用的就是这个过滤。
///
/// 只在**出错**时用得上（例如「清理后什么都没有」），成功时不往结果里塞统计：
/// 结果区要能整段复制去执行，前面挂一行统计就毁了。源页面的统计走 `duck.notify`，
/// 而这里不能——通知需要 `notifications` 能力，本仓纯函数插件零能力。
function hasStatements(text) {
  return splitStatements(text).some((s) => {
    const t = s.trim();
    return t && !t.startsWith("--") && !t.startsWith("/*") && !t.startsWith("*") && t !== "/";
  });
}

function runParam(text, params) {
  const outcome = replaceLog(text, params.schema || "");
  if (outcome.replacedCount === 0) {
    throw new Error(
      duck.t("未找到可替换的 SQL：检查日志格式，或 ? 占位符数量与参数个数是否一致"),
    );
  }
  return outcome.text;
}

function runReexec(text, params) {
  return processContent(text, {
    addSelectCheck: flag(params.add_select_check, true),
    addDelete: flag(params.add_delete, false),
    commentDelete: flag(params.comment_delete, false),
  });
}

function runClean(text) {
  const cleaned = cleanSql(text);
  if (!hasStatements(cleaned)) {
    throw new Error(duck.t("清理后没有剩下任何语句：输入里只有注释或空行"));
  }
  return cleaned;
}

function runBackup(text, params) {
  const backup = generateBackupSql(text, params.db_type || "oracle");
  if (!backup) {
    throw new Error(
      duck.t("未检测到需要备份的表（脚本里没有 INSERT / UPDATE / DELETE / DROP 等改数据的语句）"),
    );
  }
  return backup;
}

export function handle(_toolId, input) {
  const text = String(input || "");
  if (!text.trim()) throw new Error(duck.t("请粘贴或输入 SQL 内容"));
  const params = takeParams();
  switch (params.mode) {
    case "param":
      return runParam(text, params);
    case "reexec":
      return runReexec(text, params);
    case "clean":
      return runClean(text);
    case "backup":
      return runBackup(text, params);
    default:
      throw new Error(duck.t("未知的处理方式：{}", params.mode));
  }
}
