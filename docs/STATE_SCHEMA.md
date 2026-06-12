# u60-datad state schema

`u60-datad` publishes a single normalized snapshot at:

```text
/tmp/u60-datad/state.json
```

Consumers read this file. They must not call `ubus` directly.

## Shape

```json
{
  "ts": 1781201029,
  "net": {
    "type": "SA",
    "bars": 5,
    "operator": "China Mobile",
    "band": "n28",
    "nr_rsrp": -87,
    "nr_rsrq": -11,
    "nr_snr": "12.7",
    "nr_rssi": -74,
    "lte_rsrp": 0,
    "lte_rsrq": 0,
    "lte_rssi": 0,
    "lte_snr": "",
    "rssi": 0,
    "mcc": 460,
    "mnc": 0,
    "nr_pci": 0,
    "nr_cell_id": 0,
    "nr_channel": 0,
    "nr_bw": "100MHz",
    "wan_status": "ipv4_ipv6_connected"
  },
  "battery": {
    "percent": 66,
    "temp": 32,
    "online": 1,
    "health": 1,
    "time_to_full": 390,
    "charging": 1,
    "charger_connect": 1,
    "charger_type": 4
  },
  "clients": {
    "total": 0,
    "wifi": 0,
    "lan": 0
  },
  "traffic": {
    "rx_speed": 1260,
    "tx_speed": 1081,
    "max_rx_speed": 15879,
    "max_tx_speed": 13243,
    "rx_bytes": 11569922,
    "tx_bytes": 10832964,
    "session_time": 11162
  },
  "qos": {
    "qci": 9,
    "ambr_dl": 300.0,
    "ambr_ul": 100.0,
    "ambr_dl_raw": 300,
    "ambr_ul_raw": 100,
    "ambr_dl_unit": 6,
    "ambr_ul_unit": 6
  },
  "system": {
    "uptime": 11202,
    "cpu_temp": 41,
    "mem_used_pct": 52,
    "model": "ZTE U60Pro",
    "hostname": "ZTE-U60Pro",
    "fw": "OpenWrt 23.05.4 r24012-d8dd03c46f"
  }
}
```

## Source mapping

| snapshot | source |
|----------|--------|
| `net.*` | `zte_nwinfo_api nwinfo_get_netinfo` |
| `net.wan_status` | `zwrt_router.api router_get_status_no_auth` |
| `battery.*` | `zwrt_bsp.battery list` |
| `battery.charging/charger_*` | `zwrt_bsp.charger list` |
| `clients.*` | `zwrt_router.api router_get_user_list_num` |
| `traffic.*` | `zwrt_data get_wwandst {"source_module":"deviceui","cid":1,"type":1}` |
| `qos.*` | tail-read `/data/logfs/key.log` and parse latest `qci` / `session_ambr_*` |
| `system.uptime/mem` | `system info` |
| `system.cpu_temp` | `zwrt_bsp.thermal get_cpu_temp` (`cpuss_temp`) |
| `system.model/fw` | `system board` |

## Notes

- `traffic` is realtime session data from `type:1`.
- `qos.ambr_*` is normalized to Mbps.
- `qos.ambr_*_raw` plus `qos.ambr_*_unit` are kept so consumers do not need to duplicate the vendor unit table.
