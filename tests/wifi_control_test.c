/* SPDX-License-Identifier: MIT */
#include "device_exec.h"
#include "json.h"
#include "wifi_control.h"
#include <assert.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static char psm_mode[16] = "", power_mode[16] = "";
static int main_psm = 1, guest_psm = 1, power = 18, index_base = 10, writes = 0, commit_fail = 0;
static char last_iface[32];
static int ticks_time = 100;
time_t time(time_t *out)
{
    if (out)
        *out = ticks_time;
    return ticks_time;
}
int nanosleep(const struct timespec *a, struct timespec *b)
{
    (void)a;
    (void)b;
    return 0;
}
int device_uci_get(const char *path, char *out, size_t len)
{
    const char *v = NULL;
    if (!strcmp(path, "zwrt_common_info.common_config.model_name"))
        v = "MU5252";
    else if (!strcmp(path, "wireless.main_5g.datad_psm") && psm_mode[0])
        v = psm_mode;
    else if (!strcmp(path, "wireless.wifi1.datad_txpower_dbm") && power_mode[0])
        v = power_mode;
    if (!v) {
        out[0] = 0;
        return -1;
    }
    snprintf(out, len, "%s", v);
    return 0;
}
int device_uci_set(const char *path, const char *v)
{
    if (!strcmp(path, "wireless.main_5g.datad_psm"))
        snprintf(psm_mode, sizeof psm_mode, "%s", v);
    else if (!strcmp(path, "wireless.wifi1.datad_txpower_dbm"))
        snprintf(power_mode, sizeof power_mode, "%s", v);
    else
        assert(!"unexpected UCI write");
    writes++;
    return 0;
}
int device_uci_delete(const char *path)
{
    if (!strcmp(path, "wireless.main_5g.datad_psm"))
        psm_mode[0] = 0;
    else if (!strcmp(path, "wireless.wifi1.datad_txpower_dbm"))
        power_mode[0] = 0;
    else
        assert(!"unexpected UCI deletion");
    return 0;
}
int device_uci_commit(const char *p)
{
    (void)p;
    return commit_fail ? -1 : 0;
}
int device_uci_revert(const char *p)
{
    (void)p;
    return 0;
}
int device_ubus_call(const char *service, const char *method, const char *args, char *out,
                     size_t len)
{
    assert(!strcmp(service, "uci") && !strcmp(method, "get"));
    (void)args;
    snprintf(
        out, len,
        "{\"values\":{"
        "\"wifi0\":{\"disabled\":1,\"txpower\":19},"
        "\"wifi1\":{\"disabled\":0,\"txpower\":18,\"datad_txpower_dbm\":\"%s\"},"
        "\"main_2g\":{\"disabled\":0,\"ssid\":\"main\"},"
        "\"guest_2g\":{\"disabled\":1,\"ssid\":\"guest2g\"},"
        "\"main_5g\":{\"disabled\":0,\"ssid\":\"Main "
        "\\\"quoted\\\"\",\"ifname\":\"wlan8\",\"key\":\"SECRET_TEST_VALUE\",\"datad_psm\":\"%s\"},"
        "\"guest_5g\":{\"disabled\":0,\"ssid\":\"Guest\",\"ifname\":\"wlan3\"}}}",
        power_mode, psm_mode);
    return 0;
}
int device_run_capture(const char *const argv[], char *out, size_t len)
{
    out[0] = 0;
    if (!strcmp(argv[0], "hostapd_cli")) {
        snprintf(out, len, "state=ENABLED\n");
        return 0;
    }
    assert(!strcmp(argv[0], "iw"));
    if (!strcmp(argv[1], "phy")) {
        snprintf(out, len, "* 5180 MHz [36] (23.0 dBm)\n");
        return 0;
    }
    if (!argv[2]) {
        snprintf(out, len,
                 "phy#1\n\tInterface wlan2\n\t\tifindex %d\n\t\tssid Main \"quoted\"\n\t\ttype "
                 "AP\n\t\tchannel 36 (5180 MHz), width: 160 MHz\n\t\ttxpower %d.00 "
                 "dBm\n\tInterface wlan3\n\t\tifindex %d\n\t\tssid Guest\n\t\ttype AP\n\t\tchannel "
                 "36 (5180 MHz), width: 160 MHz\n\t\ttxpower %d.00 dBm\n",
                 index_base, power, index_base + 1, power);
        return 0;
    }
    assert(!strcmp(argv[2], "wlan2") || !strcmp(argv[2], "wlan3"));
    int *psm = !strcmp(argv[2], "wlan2") ? &main_psm : &guest_psm;
    if (!strcmp(argv[3], "get")) {
        snprintf(out, len, "Power save: %s\n", *psm ? "on" : "off");
        return 0;
    }
    snprintf(last_iface, sizeof last_iface, "%s", argv[2]);
    writes++;
    if (!strcmp(argv[4], "power_save"))
        *psm = !strcmp(argv[5], "on");
    else if (!strcmp(argv[4], "txpower"))
        power = atoi(argv[6]) / 100;
    else
        assert(!"unexpected driver write");
    return 0;
}
int device_run_quiet(const char *const argv[])
{
    char out[128];
    return device_run_capture(argv, out, sizeof out);
}
static int call(const char *action, const char *params, char *out)
{
    char err[256];
    int rc = wifi_control_execute(action, params, out, 16000, err, sizeof err);
    if (rc)
        assert(json_is_valid_object(out));
    return rc;
}
int main(void)
{
    char out[16000], config[] = "/tmp/wifi-policy-config-XXXXXX";
    int config_fd = mkstemp(config);
    assert(config_fd >= 0);
    close(config_fd);
    setenv("ZWRT_DATAD_WIRELESS_CONFIG", config, 1);
    assert(call("wifi.advanced.status", "{}", out));
    assert(!strstr(out, "SECRET_TEST_VALUE"));
    assert(strstr(out, "Main \\\"quoted\\\""));
    assert(strstr(out, "\"ifname\":\"wlan2\""));
    assert(call("wifi.psm.set", "{\"section\":\"main_5g\",\"mode\":\"off\"}", out));
    assert(main_psm == 0 && guest_psm == 1);
    assert(!strcmp(last_iface, "wlan2"));
    int before = writes;
    assert(!call("wifi.psm.set", "{\"section\":\"main_5g.key\",\"mode\":\"off\"}", out));
    assert(writes == before);
    assert(!call("wifi.txpower.set_dbm", "{\"band\":\"5g\",\"dbm\":30}", out));
    assert(writes == before);
    assert(!call("wifi.txpower.set_dbm", "{\"band\":\"5g\",\"dbm\":12.5}", out));
    assert(writes == before);
    assert(call("wifi.txpower.set_dbm", "{\"band\":\"5g\",\"dbm\":12}", out));
    assert(power == 12);
    ticks_time += 5;
    wifi_runtime_tick();
    ticks_time += 5;
    wifi_runtime_tick();
    before = writes;
    main_psm = 1;
    ticks_time += 5;
    wifi_runtime_tick();
    assert(writes == before && main_psm == 1); /* no fight against another owner */
    char replacement[] = "/tmp/wifi-policy-config-XXXXXX";
    config_fd = mkstemp(replacement);
    assert(config_fd >= 0);
    close(config_fd);
    assert(rename(replacement, config) == 0);
    power = 18;
    ticks_time += 5;
    wifi_runtime_tick();
    assert(power == 18);
    ticks_time += 5;
    wifi_runtime_tick();
    assert(power == 12 && main_psm == 1);
    /* OEM power reset is corrected after its config commit; PSM is untouched. */
    index_base += 10;
    ticks_time += 5;
    wifi_runtime_tick();
    assert(main_psm == 1);
    ticks_time += 5;
    wifi_runtime_tick();
    assert(main_psm == 0 && guest_psm == 1); /* follow interface generation */
    assert(call("wifi.psm.set", "{\"section\":\"main_5g\",\"mode\":\"default\"}", out));
    main_psm = 1;
    index_base += 10;
    ticks_time += 5;
    wifi_runtime_tick();
    ticks_time += 5;
    wifi_runtime_tick();
    assert(main_psm == 1);
    assert(call("wifi.txpower.set_dbm", "{\"band\":\"5g\",\"mode\":\"oem\"}", out));
    assert(power == 18 && !power_mode[0]);
    unlink(config);
    puts("wifi control tests passed");
    return 0;
}
