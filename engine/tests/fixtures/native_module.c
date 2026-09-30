/* Minimal Lua 5.4 ABI fixture, resolved from the host rather than linked to Lua. */
#include <stddef.h>

typedef struct lua_State lua_State;
extern void lua_createtable(lua_State *, int, int);
extern const char *lua_pushlstring(lua_State *, const char *, size_t);
extern void lua_setfield(lua_State *, int, const char *);

int luaopen_external_fixture(lua_State *state) {
    const char value[] = "independent native module";
    lua_createtable(state, 0, 1);
    lua_pushlstring(state, value, sizeof(value) - 1);
    lua_setfield(state, -2, "value");
    return 1;
}
