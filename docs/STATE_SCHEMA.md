# u60-datad state schema

`u60-datad` polls ubus and publishes a single normalized snapshot, updated
atomically (~1 Hz by default), at:

```
/tmp/u60-datad/state.json
```

**Consumers read this file. They must NOT call ubus directly.** The file is on
tmpfs and replaced via `rename()`, so reads are cheap and never see a torn
write. Re-read whenever you need fresh data (it changes ~once per second).

## Shape

```json
{
  "ts": 1781201029,                       // unix time of this snapshot
  "net": {
    "type": "SA",                         // network_type (SA/NSA/LTE/...)
    "bars": 5,                            // signalbar 0..5
    "operator": "China Mobile",           // network_provider_fullname
    "band": "n28",                        // wan_active_band
    "nr_rsrp": -87, "nr_rsrq": -11,
    "nr_snr": "12.7", "nr_rssi": -74,
    "lte_rsrp": 0, "rssi": 0,
    "wan_status": "ipv4_ipv6_connected"
  },
  "battery": {
    "percent": 66, "temp": 32,            // temp in °C
    "online": 1, "health": 1,
    "time_to_full": 390,                  // minutes, -1 if unknown
    "charging": 1,                        // charge_status
    "charger_connect": 1, "charger_type": 4
  },
  "clients": { "total": 0, "wifi": 0, "lan": 0 },
  "traffic": {
    "rx_speed": 1260, "tx_speed": 1081,        // bytes/s (current)
    "max_rx_speed": 15879, "max_tx_speed": 13243,
    "rx_bytes": 11569922, "tx_bytes": 10832964, // session totals
    "session_time": 11162                       // seconds since connect
  },
  "system": {
    "uptime": 11202,
    "cpu_temp": 41,                       // °C (cpuss_temp)
    "mem_used_pct": 52,
    "model": "...", "hostname": "ZTE-U60Pro",
    "fw": "OpenWrt 23.05.4 r24012-d8dd03c46f"
  }
}
```

## Source mapping (ubus → snapshot)

| snapshot | ubus call |
|----------|-----------|
| `net.*`       | `zte_nwinfo_api nwinfo_get_netinfo` |
| `net.wan_status` | `zwrt_router.api router_get_status_no_auth` |
| `battery.*`   | `zwrt_bsp.battery list` |
| `battery.charging/charger_*` | `zwrt_bsp.charger list` |
| `clients.*`   | `zwrt_router.api router_get_user_list_num` |
| `traffic.*`   | `zwrt_data get_wwandst {"source_module":"deviceui","cid":1,"type":1}` |
| `system.uptime/mem` | `system info` |
| `system.cpu_temp`   | `zwrt_bsp.thermal get_cpu_temp` (`cpuss_temp`) |
| `system.model/fw`   | `system board` (refreshed hourly) |

## Notes / TODO

- `traffic` is realtime session counters (`type:1`). Month/total counters use a
  different `type` value — not yet mapped.
- SMS unread (`zwrt_wms`) and SIM/IMEI (`zwrt_zte_mdm.api`) are not yet polled;
  add to `build_snapshot()` as needed.
