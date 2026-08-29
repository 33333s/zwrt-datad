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
    # UCI fallback copy of the external-modem multi-SIM fields, keyed by each
    # modem's active local slot (issue #23).
    s1="${MOCK_V3T1_SLOT:-${MOCK_V3T_SLOT:-0}}"
    s2="${MOCK_V3T2_SLOT:-${MOCK_V3T_SLOT:-0}}"
    printf '%s\n' \
        "zte_nwinfo.sys_info.msim_1_${s1}_network_provider='Fixture LTE One'" \
        "zte_nwinfo.sys_info.msim_1_${s1}_network_type='LTE'" \
        "zte_nwinfo.sys_info.msim_1_${s1}_wan_active_band='LTE BAND 3'" \
        "zte_nwinfo.sys_info.msim_1_${s1}_lte_rsrp='-95'" \
        "zte_nwinfo.sys_info.msim_1_${s1}_operate_mode='ONLINE'" \
        "zte_nwinfo.sys_info.msim_2_${s2}_network_provider='Fixture LTE Two'" \
        "zte_nwinfo.sys_info.msim_2_${s2}_network_type='LTE'" \
        "zte_nwinfo.sys_info.msim_2_${s2}_wan_active_band='LTE BAND 3'" \
        "zte_nwinfo.sys_info.msim_2_${s2}_lte_rsrp='-101'" \
        "zte_nwinfo.sys_info.msim_2_${s2}_operate_mode='ONLINE'"
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
        mwan3)
            [ "${MOCK_MODEL_NAME:-}" = 'MU5252' ] || exit 1
            printf '%s\n' \
                "mwan3.globals=globals" \
                "mwan3.globals.mmx_mask='0x3F00'" \
                "mwan3.zte_mwan2=interface" \
                "mwan3.zte_mwan2.enabled='1'" \
                "mwan3.zte_mwan2.family='ipv4'" \
                "mwan3.zte_mwan2.track_method='ping'" \
                "mwan3.zte_mwan2.track_ip='1.1.1.1' '8.8.8.8'" \
                "mwan3.zte_mwan2.reliability='1'" \
                "mwan3.zte_mwan2.timeout='4'" \
                "mwan3.zte_mwan2.interval='3'" \
                "mwan3.zte_mwan2.down='5'" \
                "mwan3.zte_mwan2.up='2'" \
                "mwan3.zte_mwan2_m1=member" \
                "mwan3.zte_mwan2_m1.interface='zte_mwan2'" \
                "mwan3.zte_mwan2_m1.metric='10'" \
                "mwan3.zte_mwan2_m1.weight='3'" \
                "mwan3.balanced=policy" \
                "mwan3.balanced.last_resort='default'" \
                "mwan3.balanced.use_member='zte_mwan2_m1'" \
                "mwan3.default_rule_v4=rule" \
                "mwan3.default_rule_v4.family='ipv4'" \
                "mwan3.default_rule_v4.proto='all'" \
                "mwan3.default_rule_v4.dest_ip='0.0.0.0/0'" \
                "mwan3.default_rule_v4.use_policy='balanced'" \
                "mwan3.default_rule_v4.sticky='0'" \
                "mwan3.default_rule_v4.logging='1'"
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
        zwrt_router.icgmwan.IcgDevId)
            [ "${MOCK_MODEL_NAME:-}" = 'MU5252' ] || exit 1
            printf '%s\n' 'fixture-icg-id'
            ;;
        zwrt_router.icgmwan.residual_flow)
            [ "${MOCK_MODEL_NAME:-}" = 'MU5252' ] || exit 1
            printf '%s\n' '51943409341'
            ;;
        zwrt_router.icgmwan.count_flow_today)
            [ "${MOCK_MODEL_NAME:-}" = 'MU5252' ] || exit 1
            printf '%s\n' '32386704'
            ;;
        mwan3.zte_mwan2) printf '%s\n' 'interface' ;;
        mwan3.zte_mwan2_m1) printf '%s\n' 'member' ;;
        mwan3.balanced) printf '%s\n' 'policy' ;;
        mwan3.default_rule_v4) printf '%s\n' 'rule' ;;
        *) exit 1 ;;
    esac
    exit 0
fi

case "${1:-}" in
    set|commit|revert|add_list|del_list|delete) exit 0 ;;
esac

exit 1
