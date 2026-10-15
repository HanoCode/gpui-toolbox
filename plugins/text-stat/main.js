// Duck Toolbox JS 插件契约：
//   - 入口文件固定为 main.js，导出 handle(toolId, input)；
//   - input 当前恒为空字符串（工作区画布接入后传用户输入）；
//   - 返回值是字符串产出，宿主以 toast 呈现（超 240 字截断展示）；
//   - 全局对象 duck 提供宿主能力与取词（未声明的能力调用会抛 JS 异常）：
//       duck.id / duck.dataDir
//       duck.locale                     // 当前语言 tag，如 "en" / "zh-Hans"
//       duck.t(source, ...args)         // 用本插件 locales/<tag>.toml 取词；
//                                       // miss 返回源串；args 按顺序填 {}
//       duck.notify(message)
//       duck.readClipboard() / duck.writeClipboard(text)
//       duck.readFile(relative) / duck.writeFile(relative, contents)  // 相对 duck.dataDir
//       duck.fetch(url, { method?, headers?, body?, timeoutMs? }) -> { status, ok, headers, body }
//         // 只在清单 [capabilities] 的 network_hosts 里声明过的主机上可用；
//         // 仅 https（回环地址可用 http）；不跟随跨域重定向；响应体上限 1 MiB。
//     console.log/info/warn/error 进宿主日志（target: duck_plugin）。
//   - 沙箱上限：32 MiB 内存 / 256 KiB 栈 / 10 秒执行。
//
// 翻译：插件自带译文放同目录 `locales/<tag>.toml`（键＝中文源串），见 locales/en.toml；
// 声明段在 plugin.toml 的 `[i18n]`。给用户看的文案用 duck.t(...) 包一层即可跟随
// 宿主的语言开关——宿主每次执行都会按当时的语言重新装载本目录的 locales/。

export function handle(toolId, input) {
  const text = duck.readClipboard();

  const chars = [...text].length;
  const trimmed = text.trim();
  const words = trimmed === "" ? 0 : trimmed.split(/\s+/).length;
  const lines = text === "" ? 0 : text.split(/\r\n|\r|\n/).length;

  const report = `chars=${chars} words=${words} lines=${lines}`;
  duck.writeFile("last-report.txt", report);
  duck.notify(duck.t("报告已写入 {}", "last-report.txt"));

  console.log(`text-stat: ${report}`);
  return report;
}
