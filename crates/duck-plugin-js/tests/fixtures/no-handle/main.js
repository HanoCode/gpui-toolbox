// JS 执行轨测试夹具：缺少 handle 导出（应报清单级契约错误）。
export function wrongName() {
  return "no handle here";
}
