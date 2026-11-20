import { integer } from "./datad-models.js";
export const TASK_ACTIONS = {
  "device.reboot": "重启设备",
  "device.poweroff": "关闭设备",
  "cellular.connect": "连接蜂窝网络",
  "cellular.disconnect": "断开蜂窝网络",
  "network.set_mode": "切换网络模式",
  "sim.set_slot": "切换 SIM 卡",
  "cellular.set": "数据漫游",
  "wifi.set_module": "Wi-Fi 开关",
  "wifi.set_chip": "关闭访客 Wi-Fi",
  "nfc.set": "NFC 开关",
  "sms.send_scheduled": "发送短信",
  "sms.forward.device_info": "发送设备信息",
  "cell.lock_lte": "锁定 4G 小区",
  "cell.lock_nr": "锁定 5G 小区",
  "cell.unlock_all": "解除小区锁定",
};
export const TASK_MODES = {
  WL_AND_5G: "自动",
  LTE_AND_5G: "仅 NSA",
  Only_5G: "仅 5G SA",
  WCDMA_AND_LTE: "4G / 3G",
  Only_LTE: "仅 4G",
  Only_WCDMA: "仅 3G",
};
export function taskPayload(form, view) {
  const { id, time, repeat_daily, action } = form,
    f = form.params ?? {};
  if (
    !id ||
    id !== id.trim() ||
    new TextEncoder().encode(id).length > 80 ||
    /[\x00-\x1f\x7f-\x9f]/.test(id)
  )
    throw new Error("任务名称须为 1–80 字节，不含首尾空格和控制字符");
  if (!/^(?:[01]\d|2[0-3]):[0-5]\d$/.test(time))
    throw new Error("时间格式须为 HH:mm");
  if (!(view.actions ?? []).includes(action) || !TASK_ACTIONS[action])
    throw new Error("设备不支持此任务类型");
  const params = {},
    n = (key, min, max, label) =>
      integer(String(f[key] ?? ""), min, max, label);
  switch (action) {
    case "network.set_mode":
      if (!view.network_modes.includes(f.mode))
        throw new Error("请选择支持的网络模式");
      params.mode = f.mode;
      break;
    case "sim.set_slot":
      params.slot = n("slot", 1, 2, "SIM 卡槽");
      break;
    case "cellular.set":
      params.roaming = n("roaming", 0, 1, "漫游开关");
      break;
    case "wifi.set_module":
      params.enabled = n("enabled", 0, 1, "Wi-Fi 开关");
      break;
    case "nfc.set":
      if (!["true", "false"].includes(String(f.enabled)))
        throw new Error("请选择 NFC 状态");
      params.enabled = String(f.enabled) === "true";
      break;
    case "wifi.set_chip":
      if (!["chip1", "chip2"].includes(f.chip))
        throw new Error("请选择访客频段");
      Object.assign(params, { chip: f.chip, guest_enabled: 0 });
      break;
    case "sms.send_scheduled":
      if (!/^\+?\d{3,32}$/.test(f.number ?? "") || (f.number ?? "").length > 32)
        throw new Error("请填写有效收件号码");
      if (
        !f.text?.trim() ||
        f.text.length > 280 ||
        /[\x00-\x09\x0b-\x1f\x7f-\x9f]/.test(f.text)
      )
        throw new Error("短信须为 1–280 字符，不含异常控制字符");
      Object.assign(params, { number: f.number, text: f.text });
      break;
    case "cell.lock_lte":
    case "cell.lock_nr": {
      const nr = action === "cell.lock_nr";
      params.pci = String(n("pci", 0, nr ? 1007 : 503, "PCI"));
      const key = nr ? "arfcn" : "earfcn";
      params[key] = String(n(key, 1, nr ? 3279165 : 262143, "频点"));
      if (nr) params.band = String(n("band", 1, 512, "NR 频段"));
      break;
    }
  }
  return { id, time, repeat_daily: !!repeat_daily, action, params };
}
export const RECOVERY_STATUS = {
  disabled: "未开启",
  manual_pause: "手动断网，已暂停",
  waiting: "等待检测",
  waiting_cellular: "等待有效蜂窝网络",
  online: "网络正常",
  offline: "检测失败，正在确认",
  cooldown: "冷却中",
  limit_reached: "已达本轮恢复上限",
  redialing: "已尝试重新拨号",
  rebooting: "正在请求重启",
  clock_unavailable: "设备时间异常，已暂停",
  storage_failed: "保存异常，已暂停",
};
export const REASONS = {
  smtp_auth_failed: "邮箱认证失败，请检查账号和授权码",
  smtp_auth_temporary: "邮件服务暂时拒绝认证",
  source_unavailable: "未能读取设备短信",
  clock_unavailable: "设备时间异常",
  device_clock_unavailable: "设备时间异常",
  rate_limited: "设备 SIM 短信发送次数已达上限",
  self_forward_blocked: "已阻止发回来源号码",
  message_too_long: "消息超出发送长度限制",
  storage_failed: "设备保存失败",
  forward_storage_failed: "设备保存失败",
  delivery_failed: "投递失败，请检查配置与设备网络",
  execution_failed: "设备执行失败或未确认结果",
  execution_timeout: "执行超时，结果未确认",
  unsupported_action: "设备不支持此动作",
  invalid_parameters: "设备拒绝任务参数",
  reboot_schedule_conflict: "与现有定时重启冲突",
  awaiting_probe: "命令已提交，等待后续检测",
  recovery_cancelled: "已因手动操作或配置变化取消",
  probe_rate_limited: "检测过于频繁",
  forward_disabled: "通知转发未启用或未配置渠道",
  idle: "暂无投递",
  sent: "发送成功",
  disabled: "转发未开启",
  power_unavailable: "电池状态暂不可用",
  sms_list_unavailable: "暂时无法读取短信",
};
export function recoveryPayload(form) {
  const interval = integer(form.interval_seconds, 30, 120, "检测间隔");
  if (![30, 60, 120].includes(interval))
    throw new Error("检测间隔须为 30、60 或 120 秒");
  return {
    enabled: !!form.enabled,
    interval_seconds: interval,
    failure_threshold: integer(form.failure_threshold, 3, 10, "连续失败次数"),
    cooldown_seconds: integer(form.cooldown_minutes, 3, 60, "冷却分钟") * 60,
    max_redials: integer(form.max_redials, 1, 5, "拨号上限"),
    reboot_after_failures: !!form.reboot_after_failures,
  };
}
