// duck.locale / duck.t 的取值夹具：把结果原样回传，便于逐项断言。
//
// 三条覆盖：① `duck.locale` 等于宿主注入的 tag；② `duck.t` 命中插件自己的译文；
// ③ `duck.t` 带 `...args` 时按顺序填 `{}`；④ 没登记的源串原样返回。
export function handle(toolId, input) {
  const plain = duck.t("文本统计");
  const filled = duck.t("报告已写入 {}", "out.txt");
  const missing = duck.t("这条没翻");
  return [duck.locale, plain, filled, missing].join("|");
}
