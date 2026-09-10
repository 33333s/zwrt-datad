#define main datad_program_main
#include "../src/main.c"
#undef main
#include <assert.h>

int main(void)
{
    char dir[] = "/tmp/datad-custom-test.XXXXXX", path[256], file[256];
    assert(mkdtemp(dir));
    snprintf(path, sizeof path, "%s/123", dir); assert(mkdir(path, 0700) == 0);
    snprintf(path, sizeof path, "%s/123/comm", dir);
    FILE *fp = fopen(path, "w"); assert(fp); fputs("icg-client\n", fp); fclose(fp);
    snprintf(file, sizeof file, "%s/status.json", dir);
    setenv("ZWRT_DATAD_PROC_ROOT", dir, 1);
    setenv("ZWRT_DATAD_ICG_STATUS_PATH", file, 1);
    fp = fopen(file, "w"); assert(fp);
    fprintf(fp, "{\"schema\":1,\"pid\":123,\"timestamp\":%ld,\"lanes\":["
        "{\"local_ip\":\"127.0.0.1\",\"server_ip\":\"192.0.2.1\",\"server_port\":19004,\"online\":true,\"rx_bps\":10,\"tx_bps\":20,\"rtt_us\":1000,\"sent_packets\":100,\"lost_packets\":2,\"uptime_seconds\":90},"
        "{\"local_ip\":\"127.0.0.1\",\"server_ip\":\"192.0.2.1\",\"server_port\":19004,\"online\":true,\"rx_bps\":30,\"tx_bps\":40,\"rtt_us\":3000,\"sent_packets\":300,\"lost_packets\":6,\"uptime_seconds\":10}]}\n", (long)time(NULL));
    fclose(fp);
    char output[16384]; struct buf b = {output, sizeof output, 0};
    assert(emit_custom_aggregation(&b, 1, "SMULTIWAN"));
    assert(strstr(output, "\"quic_tunnel_count\":2"));
    assert(strstr(output, "\"path_count\":1"));
    assert(strstr(output, "\"rx_bps\":40,\"tx_bps\":60"));
    assert(strstr(output, "\"remaining_bytes\":null"));
    assert(strstr(output, "\"packet_loss_percent\":2.000"));
    assert(strstr(output, "\"uptime_seconds\":90"));
    assert(!strstr(output, "tcp_tunnel_count"));
    fp = fopen(file, "w"); assert(fp); fputs("{\"schema\":1,\"pid\":123,\"timestamp\":1}\n", fp); fclose(fp);
    b.len = 0;
    assert(emit_custom_aggregation(&b, 1, "SMULTIWAN"));
    assert(strstr(output, "\"telemetry_fresh\":false"));
    assert(strstr(output, "\"online\":false"));
    assert(strstr(output, "\"quic_tunnel_count\":0"));
    // Missing telemetry must not retain the previous online snapshot.
    unlink(file);
    b.len = 0;
    assert(emit_custom_aggregation(&b, 1, "SMULTIWAN"));
    assert(strstr(output, "\"telemetry_fresh\":false"));
    assert(strstr(output, "\"path_count\":0"));
    // A stopped custom client must allow the stock telemetry path.
    unlink(path);
    b.len = 0;
    assert(!emit_custom_aggregation(&b, 1, "SMULTIWAN"));
    unlink(file); unlink(path);
    snprintf(path, sizeof path, "%s/123", dir); rmdir(path); rmdir(dir);
    puts("custom aggregation OK");
}
