// 示例插件（JS 执行轨测试夹具）。
// 契约：export function handle(toolId, input) -> string；宿主能力走全局 duck 对象。
export function handle(toolId, input) {
  console.log("sample tool ran for", toolId);
  if (input === "deny") {
    try {
      duck.readFile("outside/secret.txt");
      return "unexpected: read should have been denied";
    } catch (error) {
      return "denied: " + error;
    }
  }
  duck.notify("已处理 1 项");
  return "handled: " + toolId + ":" + input;
}
