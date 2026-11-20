import test from "node:test";
import assert from "node:assert/strict";
import { createToastManager } from "../web/toasts.js";
const entries = (manager) => {
  const state = manager.store.getState();
  return [...state.notifications, ...state.queue];
};

test("toast manager deduplicates visible and queued messages and replaces an operation result", () => {
  const manager = createToastManager();
  const first = manager.info("配置已提交");
  assert.equal(manager.info("配置已提交"), first);
  assert.equal(entries(manager).length, 1);
  manager.error("结果尚未确认", { id: first });
  assert.equal(entries(manager).length, 1);
  assert.equal(entries(manager)[0].message, "结果尚未确认");
  assert.equal(entries(manager)[0].role, "alert");
  assert.equal(entries(manager)[0].autoClose, 8000);
  manager.dismiss(first);
  assert.equal(entries(manager).length, 0);
});

test("toast queues stay bounded and beginning a new action can clear previous results", () => {
  const manager = createToastManager();
  for (let i = 0; i < 20; i++) manager.info(`结果 ${i}`);
  assert.equal(manager.store.getState().notifications.length, 3);
  assert.equal(entries(manager).length, 12);
  assert.equal(entries(manager).at(-1).message, "结果 19");
  manager.clear();
  assert.equal(entries(manager).length, 0);
  assert.equal(manager.info(""), null);
});

test("toast hosts use isolated stores and retain explicit dismissal/timing choices", () => {
  const first = createToastManager(), second = createToastManager();
  first.warning("请先核对设备文件", { autoClose: false });
  assert.equal(entries(first)[0].autoClose, false);
  assert.equal(entries(first)[0]["data-kind"], "warning");
  assert.equal(entries(second).length, 0);
  second.success("保存成功");
  assert.equal(entries(second)[0].role, "status");
  assert.equal(entries(second)[0].autoClose, 4000);
});
