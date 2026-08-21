/* SPDX-License-Identifier: MIT */
#include "json.h"

#include <assert.h>
#include <string.h>

int main(void)
{
    char out[512];

    assert(json_is_valid_object("{}"));
    assert(json_is_valid_object(" {\"action\":\"device.reboot\",\"params\":{}} \n"));
    assert(json_is_valid_object("{\"nested\":[true,false,null,-1.25e+2,{\"x\":\"\\u4e2d\"}]}"));
    assert(!json_is_valid_object("{\"action\":\"device.reboot\""));
    assert(!json_is_valid_object("{\"action\":\"device.reboot\"} trailing"));
    assert(!json_is_valid_object("[]"));
    assert(!json_is_valid_object("{\"x\":01}"));
    assert(!json_is_valid_object("{\"x\":\"bad\\qescape\"}"));

    assert(json_merge_objects(
        "{\"enable\":1,\"roam_enable\":0,\"nested\":{\"value\":[1,2]},\"profile_id\":7}",
        "{\"roam_enable\":1,\"source_module\":\"WEBUI\",\"cid\":1}",
        out, sizeof out));
    assert(strcmp(out,
        "{\"enable\":1,\"nested\":{\"value\":[1,2]},\"profile_id\":7,"
        "\"roam_enable\":1,\"source_module\":\"WEBUI\",\"cid\":1}") == 0);

    assert(json_merge_objects("{}", "{\"enabled\":false}", out, sizeof out));
    assert(strcmp(out, "{\"enabled\":false}") == 0);
    assert(!json_merge_objects("not-json", "{}", out, sizeof out));
    assert(!json_merge_objects("{}", "{}", out, 2));
    return 0;
}
