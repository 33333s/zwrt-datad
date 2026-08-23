#!/bin/sh
set -eu

if [ -n "${MOCK_CALL_LOG:-}" ]; then
    printf 'uci\t%s\n' "$*" >>"$MOCK_CALL_LOG"
fi

case "$*" in
    *__mock_fail__*) exit 1 ;;
esac

if [ "${1:-}" = "-q" ] && [ "${2:-}" = "show" ] && [ "${3:-}" = "zte_nwinfo" ]; then
    printf '%s\n' \
        "zte_nwinfo.sys_info.network_type='SA'" \
        "zte_nwinfo.signal_strength.signalbar='4'" \
        "zte_nwinfo.plmn_info.network_provider_fullname='Fixture TopFlow Mobile'" \
        "zte_nwinfo.wan_active_band.GWLSA_band='n78'" \
        "zte_nwinfo.wan_active_band.odu_nrband='78'" \
        "zte_nwinfo.signal_strength.nr5g_rsrp='-91'" \
        "zte_nwinfo.signal_strength.nr5g_rsrq='-12'" \
        "zte_nwinfo.signal_strength.nr5g_snr='17.0'" \
        "zte_nwinfo.plmn_info.rmcc='460'" \
        "zte_nwinfo.plmn_info.rmnc='1'" \
        "zte_nwinfo.cell_info.nr5g_pci='321'" \
        "zte_nwinfo.cell_info.nr5g_cellid='123456'" \
        "zte_nwinfo.cell_info.nr5g_action_channel='633984'" \
        "zte_nwinfo.cell_info.nr5g_bandwidth='100'" \
        "zte_nwinfo.sys_info.net_select='Only_5G'" \
        "zte_nwinfo.band_lock.nr5g_sa_band_lock='78'" \
        "zte_nwinfo.band_lock.nr5g_nsa_band_lock=''" \
        "zte_nwinfo.band_lock.lte_ext_band_lock='1,3'"
    exit 0
fi

if [ "${1:-}" = "-q" ] && [ "${2:-}" = "show" ]; then
    case "${3:-}" in
        zwrt_zte_mdm)
            printf '%s\n' \
                "zwrt_zte_mdm.sim_info.sim_iccid='8986000000000000000'" \
                "zwrt_zte_mdm.sim_info.sim_imsi='460000000000001'" \
                "zwrt_zte_mdm.sim_info.msisdn='10086'" \
                "zwrt_zte_mdm.sim_info.current_sim_slot='1'" \
                "zwrt_zte_mdm.sim_info.sim_states='ready'" \
                "zwrt_zte_mdm.sim_info.modem_main_state='online'" \
                "zwrt_zte_mdm.sim_info.pin_status='disabled'" \
                "zwrt_zte_mdm.device_info.imei='860000000000001'" \
                "zwrt_zte_mdm.device_info.modem_msn='fixture-msn'"
            exit 0
            ;;
        zwrt_common_info)
            printf '%s\n' \
                "zwrt_common_info.common_config.wa_inner_version='BD_FIXTURE'" \
                "zwrt_common_info.common_config.model_name='MU5252'"
            exit 0
            ;;
        network)
            printf '%s\n' \
                "network.lan.ipaddr='192.168.0.1'" \
                "network.lan.netmask='255.255.255.0'"
            exit 0
            ;;
        dhcp)
            printf '%s\n' \
                "dhcp.lan.ignore='0'" \
                "dhcp.lan.zte_start='192.168.0.2'" \
                "dhcp.lan.zte_end='192.168.0.253'" \
                "dhcp.lan.leasetime='12h'"
            exit 0
            ;;
        zwrt_data_commit)
            printf '%s\n' \
                "zwrt_data_commit.wwancid1dst.day_rx_bytes='100'" \
                "zwrt_data_commit.wwancid1dst.month_rx_bytes='1000'" \
                "zwrt_data_commit.wwancid1dst.total_rx_bytes='10000'"
            exit 0
            ;;
    esac
fi

if [ "${1:-}" = "-q" ] && [ "${2:-}" = "get" ]; then
    case "${3:-}" in
        wireless.main_2g.ssid) printf '%s\n' 'Fixture 2G' ;;
        wireless.main_2g.key) printf '%s\n' 'fixture-password' ;;
        wireless.main_2g.encryption) printf '%s\n' 'sae-mixed' ;;
        wireless.main_2g.disabled) printf '%s\n' '0' ;;
        wireless.main_5g.ssid) printf '%s\n' 'Fixture 5G' ;;
        wireless.main_5g.key) printf '%s\n' 'fixture-password' ;;
        wireless.main_5g.encryption) printf '%s\n' 'sae-mixed' ;;
        wireless.main_5g.disabled) printf '%s\n' '0' ;;
        zte_nwinfo.sys_info.network_type) printf '%s\n' 'SA' ;;
        zte_nwinfo.signal_strength.signalbar) printf '%s\n' '4' ;;
        zte_nwinfo.plmn_info.network_provider_fullname) printf '%s\n' 'Fixture TopFlow Mobile' ;;
        zte_nwinfo.wan_active_band.GWLSA_band) printf '%s\n' 'n78' ;;
        zte_nwinfo.wan_active_band.odu_nrband) printf '%s\n' '78' ;;
        zte_nwinfo.signal_strength.nr5g_rsrp) printf '%s\n' '-91' ;;
        zte_nwinfo.signal_strength.nr5g_rsrq) printf '%s\n' '-12' ;;
        zte_nwinfo.signal_strength.nr5g_snr) printf '%s\n' '17.0' ;;
        zte_nwinfo.plmn_info.rmcc) printf '%s\n' '460' ;;
        zte_nwinfo.plmn_info.rmnc) printf '%s\n' '1' ;;
        zte_nwinfo.cell_info.nr5g_pci) printf '%s\n' '321' ;;
        zte_nwinfo.cell_info.nr5g_cellid) printf '%s\n' '123456' ;;
        zte_nwinfo.cell_info.nr5g_action_channel) printf '%s\n' '633984' ;;
        zte_nwinfo.cell_info.nr5g_bandwidth) printf '%s\n' '100' ;;
        zte_nwinfo.sys_info.net_select) printf '%s\n' 'Only_5G' ;;
        zte_nwinfo.band_lock.nr5g_sa_band_lock) printf '%s\n' '78' ;;
        zte_nwinfo.band_lock.nr5g_nsa_band_lock) printf '%s\n' '' ;;
        zte_nwinfo.band_lock.lte_ext_band_lock) printf '%s\n' '1,3' ;;
        zwrt_deviceui.Device.fan_switch_status)
            [ "${MOCK_MODEL_NAME:-}" = 'MU5252' ] || exit 1
            printf '%s\n' '1'
            ;;
        zwrt_deviceui.Device.liquid_cooling_switch_status)
            [ "${MOCK_MODEL_NAME:-}" = 'MU5252' ] || exit 1
            printf '%s\n' '0'
            ;;
        zwrt_router.network.opms_wan_mode)
            [ "${MOCK_MODEL_NAME:-}" = 'MU5252' ] || exit 1
            printf '%s\n' 'SMULTIWAN'
            ;;
        *) exit 1 ;;
    esac
    exit 0
fi

case "${1:-}" in
    set|commit|revert|add_list|del_list) exit 0 ;;
esac

exit 1
