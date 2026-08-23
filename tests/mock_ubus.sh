#!/bin/sh
# Minimal ubus fixture used by CI. It records calls and returns representative
# payloads for the state collector and private control API.
set -eu

[ "$1" = "-v" ] && {
    shift
    [ "${1:-}" = "list" ] || exit 1
    printf '%s\n' "'system' @fixture" '    "board":{}' "'zte_nwinfo_api' @fixture" '    "nwinfo_get_netinfo":{}' '    "nwinfo_get_msim_netinfo":{}'
    exit 0
}
[ "$1" = "list" ] && {
    printf '%s\n' system zte_nwinfo_api zwrt_data zwrt_zte_mdm.api
    exit 0
}
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
        model="${MOCK_MODEL_NAME:-MU5250}"
        market='U60 Pro'
        [ "$model" = 'MC8532B' ] && market='G5 Pro'
        [ "$model" = 'MU5252' ] && market='TopFlow'
        [ "$model" = 'MC7523' ] && market='G5 Max WiFi'
        printf '{"manufacturer":"ZTE","model_name":"%s","hardware_version":"%s_HW1.0","device_market_name":"%s","wa_inner_version":"TEST-BUILD"}\n' \
            "$model" "$model" "$market"
        ;;
    zwrt_zte_mdm.api:get_imei)
        printf '%s\n' '{"imei":"860000000000001"}'
        ;;
    zwrt_zte_mdm.api:get_sim_info)
        if [ "${MOCK_ENCRYPTED_SIM:-0}" = '1' ]; then
            printf '{"sim_iccid":"8986000000000000000","sim_imsi":"mfmROY/c1MUtLKr/TBqrpmuNTnYHpNc70Cgl0CWlaUVXuVyARQNas+V9SA==","msisdn":"rhy1F7ceRHu/QJNfxD+UH4QG8yUH2qnHraq5wLbZUqXYiQ31HbkQbal=","sim_states":"ready","current_sim_slot":%s,"support_dual_sim":1}\n' \
                "${MOCK_SIM_SLOT:-1}"
        else
            printf '{"sim_iccid":"8986000000000000000","sim_imsi":"460000000000001","msisdn":"10086","sim_states":"ready","current_sim_slot":%s,"support_dual_sim":1}\n' \
                "${MOCK_SIM_SLOT:-1}"
        fi
        ;;
    zwrt_zte_mdm.api:get_v3t_sim_info)
        printf '%s\n' '{"v3t_1_modem_main_state":"modem_init_complete","v3t_1_sim_imsi":"460000000000003","v3t_1_sim_iccid":"8986000000000000003","v3t_1_msisdn":"10010","v3t_1_imei":"860000000000003","v3t_1_st_slot":"0","v3t_2_modem_main_state":"modem_init_complete","v3t_2_sim_imsi":"460000000000005","v3t_2_sim_iccid":"8986000000000000005","v3t_2_msisdn":"10011","v3t_2_imei":"860000000000005","v3t_2_st_slot":"0"}'
        ;;
    zte_nwinfo_api:nwinfo_get_netinfo)
        [ "${MOCK_NWINFO_FAIL:-0}" = '1' ] && exit 1
        printf '%s\n' '{"network_type":"SA","signalbar":4,"network_provider_fullname":"Fixture Mobile","wan_active_band":"n78","nr5g_action_band":"78","nr5g_rsrp":-90,"nr5g_rsrq":-11,"nr5g_snr":"18.0","rmcc":460,"rmnc":0,"net_select":"WL_AND_5G","nr5g_sa_band_lock":"78","nr5g_nsa_band_lock":"","lte_band":"1,3"}'
        ;;
    zte_nwinfo_api:nwinfo_get_msim_netinfo)
        printf '%s\n' '{"msim_1_0_net_select":"Only_LTE","msim_1_0_network_type":"LTE","msim_1_0_rplmn_num":"46000","msim_1_0_network_provider":"Fixture LTE One","msim_1_0_wan_active_band":"LTE BAND 3","msim_1_0_signalbar":"4","msim_1_0_cell_id":"1001","msim_1_0_wan_active_channel":"1300","msim_1_0_lte_pci":"31","msim_1_0_lte_rsrp":"-95","msim_1_0_lte_rsrq":"-10","msim_1_0_lte_rssi":"-65","msim_1_0_lte_snr":"7.0","msim_1_0_operate_mode":"ONLINE","msim_1_0_lte_bandwidth":"20","msim_2_0_net_select":"Only_LTE","msim_2_0_network_type":"LTE","msim_2_0_rplmn_num":"46001","msim_2_0_network_provider":"Fixture LTE Two","msim_2_0_wan_active_band":"LTE BAND 3","msim_2_0_signalbar":"3","msim_2_0_cell_id":"1002","msim_2_0_wan_active_channel":"1650","msim_2_0_lte_pci":"32","msim_2_0_lte_rsrp":"-101","msim_2_0_lte_rsrq":"-13","msim_2_0_lte_rssi":"-73","msim_2_0_lte_snr":"1.0","msim_2_0_operate_mode":"ONLINE","msim_2_0_lte_bandwidth":"20"}'
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
    zwrt_web:web_login_info)
        printf '%s\n' '{"zte_web_sault":"fixture-salt","login_fail_num":0}'
        ;;
    zwrt_router.api:router_get_wifi_isolate)
        printf '%s\n' '{"wifimain24_wifimain5_enable":1,"other_option":7}'
        ;;
    zwrt_router.api:router_set_wifi_isolate|zwrt_router.api:router_set_wan_mtu)
        printf '%s\n' '{"result":"success"}'
        ;;
    zwrt_data:get_wwaniface)
        printf '%s\n' '{"enable":1,"roam_enable":0,"connect_mode":"auto","connect_status":"ipv4_ipv6_connected","ipv4_dev_name":"fixture0","ipv6_dev_name":"fixture0","pdp_type":"IPV4V6","profile_id":7}'
        ;;
    zwrt_data:get_wwandst)
        printf '%s\n' '{"real_time":12,"real_tx_bytes":120,"real_rx_bytes":240,"real_tx_speed":10,"real_rx_speed":20,"real_max_tx_speed":30,"real_max_rx_speed":40,"day_tx_bytes":120,"day_rx_bytes":240,"month_tx_bytes":120,"month_rx_bytes":240,"total_tx_bytes":120,"total_rx_bytes":240}'
        ;;
    zwrt_bsp.thermal:get_cpu_temp)
        printf '%s\n' '{"cpuss_temp":42}'
        ;;
    network.interface.zte_mwan2:status|network.interface.zte_mwan2_6:status|network.interface.zte_mwan3:status|network.interface.zte_mwan3_6:status|network.interface.zte_mwan4:status|network.interface.zte_mwan4_6:status)
        printf '%s\n' '{"up":true,"proto":"dhcp","l3_device":"fixture0","ipv4-address":[],"ipv6-address":[],"dns-server":[]}'
        ;;
    *)
        printf '%s\n' '{"result":"success"}'
        ;;
esac
