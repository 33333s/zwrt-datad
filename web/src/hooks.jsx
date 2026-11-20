import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useId,
} from "react";
import { toast } from "../toasts.js";
import { useServices } from "./ui.jsx";

// A page owns its requests. A response from a disposed page cannot initialize a new form.
export function usePageApi() {
  const { api, observeSnapshot } = useServices();
  const scope = useMemo(
    () => ({ controller: new AbortController(), generation: api.generation }),
    [api, api.generation],
  );
  useEffect(() => () => scope.controller.abort(), [scope]);
  return useMemo(() => {
    function check() {
      if (
        scope.controller.signal.aborted ||
        scope.generation !== api.generation
      )
        throw new DOMException("页面或会话已结束", "AbortError");
    }
    return {
      get generation() {
        return scope.generation;
      },
      check,
      transaction: async (key, work) => {
        check();
        const result = await api.transaction(key, work);
        check();
        return result;
      },
      request: async (path, options = {}) => {
        check();
        const result = await api.request(path, {
          ...options,
          signal: options.signal
            ? AbortSignal.any([options.signal, scope.controller.signal])
            : scope.controller.signal,
        });
        check();
        if (path === "/state") observeSnapshot?.(result);
        return result;
      },
      control: async (action, params = {}, options = {}) => {
        check();
        const result = await api.control(action, params, {
          ...options,
          signal: scope.controller.signal,
        });
        check();
        return result;
      },
      sleep: (ms) =>
        new Promise((resolve, reject) => {
          check();
          const abort = () => {
            clearTimeout(timer);
            reject(new DOMException("页面已结束", "AbortError"));
          };
          const timer = setTimeout(() => {
            scope.controller.signal.removeEventListener("abort", abort);
            try {
              check();
              resolve();
            } catch (e) {
              reject(e);
            }
          }, ms);
          scope.controller.signal.addEventListener("abort", abort, {
            once: true,
          });
        }),
    };
  }, [api, scope, observeSnapshot]);
}
export function useResource(load, deps = [], interval = 0) {
  const loader = useRef(load);
  loader.current = load;
  const version = useRef(0),
    alive = useRef(true),
    [state, setState] = useState({ data: null, error: "", loading: true });
  const refresh = useCallback(async () => {
    const current = ++version.current;
    setState((s) => ({ ...s, loading: true, error: "" }));
    try {
      const data = await loader.current();
      if (alive.current && current === version.current)
        setState({ data, error: "", loading: false });
      return data;
    } catch (e) {
      if (
        alive.current &&
        current === version.current &&
        e.name !== "AbortError"
      )
        setState((s) => ({ ...s, error: e.message, loading: false }));
      return null;
    }
  }, []);
  useEffect(() => {
    alive.current = true;
    let cancelled = false,
      timer;
    async function tick() {
      if (!document.hidden) await refresh();
      if (!cancelled && interval) timer = setTimeout(tick, interval);
    }
    void tick();
    return () => {
      cancelled = true;
      alive.current = false;
      version.current++;
      clearTimeout(timer);
    };
  }, [...deps, interval, refresh]);
  return {
    ...state,
    refresh,
    setData: (data) => {
      if (alive.current) {
        version.current++;
        setState((s) => ({ ...s, data, error: "", loading: false }));
      }
    },
  };
}
export function useOperation() {
  const notificationId = "operation-" + useId();
  const active = useRef(true),
    lock = useRef(false),
    [state, setState] = useState({ busy: false, error: "", message: "" });
  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
    };
  }, []);
  const run = async (work, { write = false, message = "" } = {}) => {
    if (lock.current) return null;
    lock.current = true;
    toast.clear();
    if (active.current) setState({ busy: true, error: "", message: "" });
    try {
      const result = await work();
      if (active.current) {
        const text = typeof message === "function" ? message(result) : message;
        setState({ busy: false, error: "", message: text });
        if (text) toast.info(text, { id: notificationId });
      }
      return result ?? true;
    } catch (e) {
      if (active.current && e.name !== "AbortError") {
        const text =
          e.message +
          (write && (!e.status || e.status >= 500)
            ? "。结果未确认，请重新读取，勿直接重复提交。"
            : "");
        setState({ busy: false, error: text, message: "" });
        toast.error(text, { id: notificationId });
      }
      return null;
    } finally {
      lock.current = false;
      if (active.current) setState((s) => ({ ...s, busy: false }));
    }
  };
  return {
    ...state,
    run,
    clear: () => {
      toast.clear();
      setState((s) => ({ ...s, error: "", message: "" }));
    },
  };
}
export function useDirtyGuard(dirty) {
  const { registerLeaveGuard } = useServices();
  const current = useRef(dirty);
  current.current = dirty;
  useEffect(() => {
    const release = registerLeaveGuard?.(() => current.current);
    const before = (e) => {
      if (current.current) {
        e.preventDefault();
        e.returnValue = "";
      }
    };
    addEventListener("beforeunload", before);
    return () => {
      release?.();
      removeEventListener("beforeunload", before);
    };
  }, [registerLeaveGuard]);
}
