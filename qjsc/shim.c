/* Same compile as esp-quickjs-ng tools/qjsc-esp/qjsc_esp.c, as a function. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "quickjs.h"

static char *dup_error(JSContext *ctx)
{
    JSValue ex = JS_GetException(ctx);
    const char *msg = JS_ToCString(ctx, ex);
    JSValue stack = JS_IsObject(ex) ? JS_GetPropertyStr(ctx, ex, "stack") : JS_UNDEFINED;
    const char *st = JS_IsString(stack) ? JS_ToCString(ctx, stack) : NULL;
    const char *m = msg ? msg : "(error)", *s = st ? st : "";
    size_t n = strlen(m) + 1 + strlen(s) + 1;
    char *out = malloc(n);
    if (out) {
        strcpy(out, m);
        if (*s) {
            strcat(out, "\n");
            strcat(out, s);
        }
    }
    JS_FreeCString(ctx, st);
    JS_FreeCString(ctx, msg);
    JS_FreeValue(ctx, stack);
    JS_FreeValue(ctx, ex);
    return out;
}

static int dummy_init(JSContext *ctx, JSModuleDef *m)
{
    abort(); /* never run, we only compile */
    return -1;
}

static JSModuleDef *loader(JSContext *ctx, const char *name, void *opaque)
{
    if (strchr(name, ':')) {
        return JS_NewCModule(ctx, name, dummy_init);
    }
    JS_ThrowReferenceError(ctx, "import '%s': bundle local files into one module first", name);
    return NULL;
}

const char *sidp_qjsc_version(void)
{
    return JS_GetVersion();
}

/* src[len] must be 0. On success *out is malloc'd bytecode, else *err a malloc'd message.
 * script: an async global script (qjsc-esp -e) instead of a module. */
int sidp_qjsc_compile(const char *src, size_t len, const char *name, int strip, int script, uint8_t **out,
                      size_t *out_len, char **err)
{
    *out = NULL;
    *err = NULL;
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = rt ? JS_NewContext(rt) : NULL;
    if (!ctx) {
        if (rt) {
            JS_FreeRuntime(rt);
        }
        return -1;
    }
    JS_SetModuleLoaderFunc(rt, NULL, loader, NULL);
    int type = script ? JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_ASYNC : JS_EVAL_TYPE_MODULE;
    JSValue fn = JS_Eval(ctx, src, len, name, type | JS_EVAL_FLAG_COMPILE_ONLY);
    int ret = -1;
    if (JS_IsException(fn)) {
        *err = dup_error(ctx);
    } else {
        int flags = JS_WRITE_OBJ_BYTECODE;
        if (strip >= 1) {
            flags |= JS_WRITE_OBJ_STRIP_SOURCE;
        }
        if (strip >= 2) {
            flags |= JS_WRITE_OBJ_STRIP_DEBUG;
        }
        size_t n = 0;
        uint8_t *bc = JS_WriteObject(ctx, &n, fn, flags);
        JS_FreeValue(ctx, fn);
        if (!bc) {
            *err = dup_error(ctx);
        } else {
            *out = malloc(n ? n : 1);
            if (*out) {
                memcpy(*out, bc, n);
                *out_len = n;
                ret = 0;
            }
            js_free(ctx, bc);
        }
    }
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
    return ret;
}

void sidp_qjsc_free(void *p)
{
    free(p);
}
