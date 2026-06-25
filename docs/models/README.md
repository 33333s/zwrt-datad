# Device API Templates

后端会先读取 `device.model_name` / `hardware_version`，再在内部选择设备侧 API 模板。

当前模板列表：

- `MU5250`
  - 匹配：`model_name = MU5250`
  - 文档：[`MU5250.md`](MU5250.md)

待后续补充：

- `G5Pro` 独立模板
- 其他机型模板

说明：

- `device.api_template_supported = 1` 才表示当前机型已有正式模板。
- `device.api_template_supported = 0` 只表示后端落到了内部兼容模板，便于调试，不应当作“已经适配完成”。
