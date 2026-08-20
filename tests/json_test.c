/* SPDX-License-Identifier: MIT */
#include "json.h"

#include <assert.h>
#include <string.h>

int main(void)
{
    char out[512];

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
