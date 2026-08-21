/*
 * json.c - tiny read-only JSON value extractor. See json.h.
 *
 * SPDX-License-Identifier: MIT
 */
#include "json.h"
#include <ctype.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

static int is_ws(char c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r'; }

static const char *skip_json_string(const char *p)
{
    if (!p || *p != '"') return NULL;

    int esc = 0;
    for (p++; *p; p++) {
        if (esc) { esc = 0; continue; }
        if (*p == '\\') { esc = 1; continue; }
        if (*p == '"') return p + 1;
    }
    return NULL;
}

static const char *skip_ws_ptr(const char *p)
{
    while (p && is_ws(*p)) p++;
    return p;
}

static int is_hex_digit(char c)
{
    return (c >= '0' && c <= '9') ||
           (c >= 'a' && c <= 'f') ||
           (c >= 'A' && c <= 'F');
}

static const char *validate_json_string(const char *p)
{
    if (!p || *p != '"') return NULL;
    for (p++; *p; p++) {
        unsigned char c = (unsigned char)*p;
        if (c == '"') return p + 1;
        if (c < 0x20) return NULL;
        if (c != '\\') continue;

        p++;
        if (!*p) return NULL;
        if (*p == '"' || *p == '\\' || *p == '/' ||
            *p == 'b' || *p == 'f' || *p == 'n' ||
            *p == 'r' || *p == 't') {
            continue;
        }
        if (*p != 'u') return NULL;
        for (int i = 1; i <= 4; i++) {
            if (!p[i] || !is_hex_digit(p[i])) return NULL;
        }
        p += 4;
    }
    return NULL;
}

static const char *validate_json_number(const char *p)
{
    if (*p == '-') p++;
    if (*p == '0') {
        p++;
    } else {
        if (*p < '1' || *p > '9') return NULL;
        while (*p >= '0' && *p <= '9') p++;
    }
    if (*p == '.') {
        p++;
        if (*p < '0' || *p > '9') return NULL;
        while (*p >= '0' && *p <= '9') p++;
    }
    if (*p == 'e' || *p == 'E') {
        p++;
        if (*p == '+' || *p == '-') p++;
        if (*p < '0' || *p > '9') return NULL;
        while (*p >= '0' && *p <= '9') p++;
    }
    return p;
}

static const char *validate_json_value(const char *p, unsigned depth)
{
    p = skip_ws_ptr(p);
    if (!p || !*p || depth > 64) return NULL;

    if (*p == '"') return validate_json_string(p);
    if (*p == '-' || (*p >= '0' && *p <= '9')) return validate_json_number(p);
    if (!strncmp(p, "true", 4)) return p + 4;
    if (!strncmp(p, "false", 5)) return p + 5;
    if (!strncmp(p, "null", 4)) return p + 4;

    if (*p == '[') {
        p = skip_ws_ptr(p + 1);
        if (*p == ']') return p + 1;
        for (;;) {
            p = validate_json_value(p, depth + 1);
            if (!p) return NULL;
            p = skip_ws_ptr(p);
            if (*p == ']') return p + 1;
            if (*p != ',') return NULL;
            p = skip_ws_ptr(p + 1);
        }
    }

    if (*p == '{') {
        p = skip_ws_ptr(p + 1);
        if (*p == '}') return p + 1;
        for (;;) {
            p = validate_json_string(p);
            if (!p) return NULL;
            p = skip_ws_ptr(p);
            if (*p != ':') return NULL;
            p = validate_json_value(p + 1, depth + 1);
            if (!p) return NULL;
            p = skip_ws_ptr(p);
            if (*p == '}') return p + 1;
            if (*p != ',') return NULL;
            p = skip_ws_ptr(p + 1);
        }
    }

    return NULL;
}

int json_is_valid_object(const char *json)
{
    const char *start = skip_ws_ptr(json);
    const char *end;
    if (!start || *start != '{') return 0;
    end = validate_json_value(start, 0);
    if (!end) return 0;
    end = skip_ws_ptr(end);
    return *end == 0;
}

static const char *skip_json_value(const char *p)
{
    char stack[64];
    size_t depth = 0;

    p = skip_ws_ptr(p);
    if (!p || !*p) return NULL;
    if (*p == '"') return skip_json_string(p);
    if (*p != '{' && *p != '[') {
        while (*p && *p != ',' && *p != '}' && *p != ']' && !is_ws(*p)) p++;
        return p;
    }

    stack[depth++] = *p == '{' ? '}' : ']';
    for (p++; *p; p++) {
        if (*p == '"') {
            p = skip_json_string(p);
            if (!p) return NULL;
            p--;
            continue;
        }
        if (*p == '{' || *p == '[') {
            if (depth == sizeof stack) return NULL;
            stack[depth++] = *p == '{' ? '}' : ']';
        } else if (*p == '}' || *p == ']') {
            if (!depth || stack[depth - 1] != *p) return NULL;
            if (--depth == 0) return p + 1;
        }
    }
    return NULL;
}

static int next_object_member(const char **cursor, const char **member_start,
                              const char **member_end, const char **key_start,
                              size_t *key_len)
{
    const char *p = skip_ws_ptr(*cursor);
    const char *after_key;
    const char *value_end;

    if (*p == ',') p = skip_ws_ptr(p + 1);
    if (*p == '}') {
        *cursor = p + 1;
        return 0;
    }
    if (*p != '"') return -1;
    *member_start = p;
    *key_start = p + 1;
    after_key = skip_json_string(p);
    if (!after_key) return -1;
    *key_len = (size_t)((after_key - 1) - *key_start);
    p = skip_ws_ptr(after_key);
    if (*p != ':') return -1;
    value_end = skip_json_value(p + 1);
    if (!value_end) return -1;
    *member_end = value_end;
    p = skip_ws_ptr(value_end);
    if (*p != ',' && *p != '}') return -1;
    *cursor = p;
    return 1;
}

static int append_bytes(char *out, size_t outlen, size_t *used,
                        const char *data, size_t len)
{
    if (*used + len >= outlen) return 0;
    memcpy(out + *used, data, len);
    *used += len;
    out[*used] = 0;
    return 1;
}

static int hex4(const char **pp, uint32_t *out)
{
    const char *p = *pp;
    uint32_t v = 0;
    int d;
    for (int i = 0; i < 4; i++) {
        if (!p[i]) return 0;
        unsigned char c = (unsigned char)p[i];
        if (c >= '0' && c <= '9') d = c - '0';
        else if (c >= 'a' && c <= 'f') d = 10 + (c - 'a');
        else if (c >= 'A' && c <= 'F') d = 10 + (c - 'A');
        else return 0;
        v = (v << 4) | (uint32_t)d;
    }
    *out = v;
    *pp = p + 4;
    return 1;
}

static size_t append_utf8_codepoint_json(char *out, size_t outlen, size_t pos, uint32_t cp)
{
    if (cp <= 0x7F) {
        if (pos + 1 >= outlen) return pos;
        out[pos++] = (char)cp;
        return pos;
    }
    if (cp <= 0x7FF) {
        if (pos + 2 >= outlen) return pos;
        out[pos++] = (char)(0xC0 | (cp >> 6));
        out[pos++] = (char)(0x80 | (cp & 0x3F));
        return pos;
    }
    if (cp <= 0xFFFF) {
        if (cp >= 0xD800 && cp <= 0xDFFF) cp = 0xFFFDu;
        if (pos + 3 >= outlen) return pos;
        out[pos++] = (char)(0xE0 | (cp >> 12));
        out[pos++] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[pos++] = (char)(0x80 | (cp & 0x3F));
        return pos;
    }
    if (cp <= 0x10FFFF) {
        if (pos + 4 >= outlen) return pos;
        out[pos++] = (char)(0xF0 | (cp >> 18));
        out[pos++] = (char)(0x80 | ((cp >> 12) & 0x3F));
        out[pos++] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[pos++] = (char)(0x80 | (cp & 0x3F));
        return pos;
    }
    return pos;
}

/* Return a pointer just past the ':' of "key": , or NULL. */
static const char *find_key(const char *json, const char *key)
{
    size_t klen = strlen(key);
    int obj_depth = 0;
    int arr_depth = 0;
    int expect_key = 0;
    const char *p = json;

    while (*p) {
        char c = *p;
        if (c == '"') {
            const char *ks = p + 1;
            const char *kp = skip_json_string(p);
            if (!kp) return NULL;
            const char *ke = kp - 1;
            if (expect_key && obj_depth == 1 &&
                (size_t)(ke - ks) == klen && strncmp(ks, key, klen) == 0) {
                const char *q = kp;
                while (is_ws(*q)) q++;
                if (*q == ':') return q + 1;
            }
            p = kp;
            if (obj_depth == 1 && expect_key) expect_key = 0;
            continue;
        }
        if (c == '{') {
            obj_depth++;
            expect_key = (obj_depth == 1);
            p++;
            continue;
        }
        if (c == '}') {
            if (obj_depth > 0) obj_depth--;
            expect_key = (obj_depth == 1);
            p++;
            continue;
        }
        if (c == '[') {
            arr_depth++;
            expect_key = 0;
            p++;
            continue;
        }
        if (c == ']') {
            if (arr_depth > 0) arr_depth--;
            p++;
            continue;
        }
        if (c == ',') {
            if (obj_depth == 1 && arr_depth == 0) expect_key = 1;
            p++;
            continue;
        }
        if (c == ':') {
            expect_key = 0;
            p++;
            continue;
        }
        p++;
    }
    return NULL;
}

int json_get(const char *json, const char *key, char *out, size_t outlen)
{
    if (!json || !key || outlen == 0) return 0;
    const char *v = find_key(json, key);
    if (!v) return 0;
    while (is_ws(*v)) v++;

    size_t n = 0;
    if (*v == '"') {
        v++;
        while (*v && *v != '"' && n < outlen - 1) {
            if (*v == '\\' && v[1]) {
                v++;
                if (*v == '"') out[n++] = '"';
                else if (*v == '\\') out[n++] = '\\';
                else if (*v == '/') out[n++] = '/';
                else if (*v == 'b') out[n++] = '\b';
                else if (*v == 'f') out[n++] = '\f';
                else if (*v == 'n') out[n++] = '\n';
                else if (*v == 'r') out[n++] = '\r';
                else if (*v == 't') out[n++] = '\t';
                else if (*v == 'u') {
                    v++;
                    uint32_t cp = 0;
                    const char *q = v;
                    if (!hex4(&q, &cp)) break;
                    if (cp >= 0xD800 && cp <= 0xDBFF &&
                        q[0] == '\\' && q[1] == 'u') {
                        const char *q2 = q + 2;
                        uint32_t lo;
                        if (hex4(&q2, &lo) && lo >= 0xDC00 && lo <= 0xDFFF) {
                            cp = 0x10000u + ((cp - 0xD800u) << 10) + (lo - 0xDC00u);
                            q = q2;
                        }
                    }
                    n = append_utf8_codepoint_json(out, outlen, n, cp);
                    v = q;
                    continue;
                } else {
                    out[n++] = *v;
                }
                v++;
                continue;
            }
            out[n++] = *v++;
        }
    } else if (*v == '{' || *v == '[') {
        char open = *v, close = (open == '{') ? '}' : ']';
        int depth = 0;
        int in_str = 0;
        while (*v && n < outlen - 1) {
            char c = *v;
            if (in_str) {
                if (c == '\\' && v[1]) { out[n++] = *v++; if (n < outlen-1) out[n++] = *v++; continue; }
                if (c == '"') in_str = 0;
            } else {
                if (c == '"') in_str = 1;
                else if (c == open) depth++;
                else if (c == close) depth--;
            }
            out[n++] = *v++;
            if (!in_str && depth == 0) break;
        }
    } else {
        while (*v && *v != ',' && *v != '}' && *v != ']' && !is_ws(*v) && n < outlen - 1)
            out[n++] = *v++;
    }
    out[n] = 0;
    return 1;
}

long json_get_int(const char *json, const char *key, long def)
{
    char buf[64];
    if (!json_get(json, key, buf, sizeof buf)) return def;
    char *end;
    long v = strtol(buf, &end, 10);
    return (end == buf) ? def : v;
}

int json_merge_objects(const char *base, const char *overlay, char *out, size_t outlen)
{
    const char *objects[2] = {base, overlay};
    size_t used = 0;
    int emitted = 0;

    if (!base || !overlay || !out || outlen < 3) return 0;
    out[0] = 0;
    if (!append_bytes(out, outlen, &used, "{", 1)) return 0;

    for (size_t object_index = 0; object_index < 2; object_index++) {
        const char *cursor = skip_ws_ptr(objects[object_index]);
        if (!cursor || *cursor != '{') return 0;
        cursor++;
        for (;;) {
            const char *member_start, *member_end, *key_start;
            size_t key_len;
            char key[256];
            int rc = next_object_member(&cursor, &member_start, &member_end,
                                        &key_start, &key_len);
            if (rc == 0) break;
            if (rc < 0 || key_len >= sizeof key) return 0;
            memcpy(key, key_start, key_len);
            key[key_len] = 0;
            if (object_index == 0 && find_key(overlay, key)) continue;
            if (emitted++ && !append_bytes(out, outlen, &used, ",", 1)) return 0;
            if (!append_bytes(out, outlen, &used, member_start,
                              (size_t)(member_end - member_start))) return 0;
        }
    }
    return append_bytes(out, outlen, &used, "}", 1);
}
