// Duck Toolbox JS 插件契约：
//   - 入口文件固定为 main.js，导出 handle(toolId, input)；
//   - input 当前恒为空字符串（工作区画布接入后传用户输入）；
//   - 返回值是字符串产出，宿主以 toast 呈现（超 240 字截断展示）；
//   - 全局对象 duck 提供宿主能力（未声明的能力调用会抛 JS 异常）：
//       duck.id / duck.dataDir
//       duck.notify(message)
//       duck.readClipboard() / duck.writeClipboard(text)
//       duck.readFile(relative) / duck.writeFile(relative, contents)  // 相对 duck.dataDir
//       duck.fetch(url, { method?, headers?, body?, timeoutMs? }) -> { status, ok, headers, body }
//         // 只在清单 [capabilities] 的 network_hosts 里声明过的主机上可用；
//         // 仅 https（回环地址可用 http）；不跟随跨域重定向；响应体上限 1 MiB。
//     console.log/info/warn/error 进宿主日志（target: duck_plugin）。
//   - 沙箱上限：32 MiB 内存 / 256 KiB 栈 / 10 秒执行。

export function handle(toolId, input) {
  const text = duck.readClipboard();

  const chars = [...text].length;
  const trimmed = text.trim();
  const words = trimmed === "" ? 0 : trimmed.split(/\s+/).length;
  const lines = text === "" ? 0 : text.split(/\r\n|\r|\n/).length;

  const report = `chars=${chars} words=${words} lines=${lines}`;
  duck.writeFile("last-report.txt", report);
  duck.notify(`报告已写入 ${duck.dataDir}/last-report.txt`);

  console.log(`text-stat: ${report}`);
  return report;
}
