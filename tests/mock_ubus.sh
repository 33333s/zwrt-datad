#!/bin/sh
# Minimal ubus fixture used by CI. It records calls and returns representative
# payloads for the state collector and private control API.
set -eu

[ "$1" = "-t" ] && shift 2
[ "$1" = "call" ] && shift
service="${1:-}"
method="${2:-}"
args="${3:-}"
[ -n "$args" ] || args='{}'

if [ -n "${MOCK_CALL_LOG:-}" ]; then
    printf '%s\t%s\t%s\n' "$service" "$method" "$args" >>"$MOCK_CALL_LOG"
fi

case "$service:$method" in
    system:board)
        printf '%s\n' '{"model":"Fixture Router","hostname":"fixture","board_name":"qcom,fixture","release":{"description":"Fixture Linux"}}'
        ;;
    system:info)
        printf '%s\n' '{"uptime":123,"memory":{"total":1048576,"available":524288}}'
        ;;
    zwrt_zte_mdm.api:get_zwrt_common_info)
        printf '%s\n' '{"manufacturer":"ZTE","model_name":"MU5250","hardware_version":"MU5250_HW1.0","device_market_name":"U60 Pro","wa_inner_version":"TEST-BUILD"}'
        ;;
    zwrt_zte_mdm.api:get_imei)
        printf '%s\n' '{"imei":"860000000000001"}'
        ;;
    zwrt_zte_mdm.api:get_sim_info)
        printf '%s\n' '{"sim_iccid":"8986000000000000000","sim_imsi":"460000000000001","msisdn":"10086","sim_states":"ready","current_sim_slot":1,"support_dual_sim":1}'
        ;;
    zte_nwinfo_api:nwinfo_get_netinfo)
        printf '%s\n' '{"network_type":"SA","signalbar":4,"network_provider_fullname":"Fixture Mobile","wan_active_band":"n78","nr5g_action_band":"78","nr5g_rsrp":-90,"nr5g_rsrq":-11,"nr5g_snr":"18.0","rmcc":460,"rmnc":0,"net_select":"WL_AND_5G","nr5g_sa_band_lock":"78","nr5g_nsa_band_lock":"","lte_band":"1,3"}'
        ;;
    zwrt_wms:zwrt_wms_get_wms_capacity)
        printf '%s\n' '{"sms_dev_unread_num":1,"sms_sim_unread_num":0}'
        ;;
    zwrt_wms:zte_libwms_get_sms_data)
        printf '%s\n' '{"list":[]}'
        ;;
    zwrt_web:web_login)
        printf '%s\n' '{"result":0,"ubus_rpc_session":"fixture-session"}'
        ;;
    zwrt_data:get_wwaniface)
        printf '%s\n' '{"enable":1,"roam_enable":0,"connect_mode":"auto","pdp_type":"IPV4V6","profile_id":7}'
        ;;
    *)
        printf '%s\n' '{"result":"success"}'
        ;;
esac
