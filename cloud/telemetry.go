package main

// systemTelemetry translates the datad snapshot into the NMS v1 resource schema.
// Select fields explicitly so adding sensitive fields to /state never uploads them.
func systemTelemetry(state map[string]any) map[string]any {
	out := map[string]any{}
	s, _ := state["system"].(map[string]any)
	if n, ok := s["cpu_usage"].(float64); ok && n >= 0 && n <= 100 {
		out["cpu"] = map[string]any{"usage_percent": n}
	}
	if n, ok := s["mem_used_pct"].(float64); ok && n >= 0 && n <= 100 {
		out["memory"] = map[string]any{"usage_percent": n}
	}
	thermal, _ := state["thermal"].(map[string]any)
	if n, ok := thermal["cpu_celsius"].(float64); ok && n > 0 && n < 150 {
		out["temperature"] = map[string]any{"available": true, "cpu_celsius": n}
	}
	return out
}
