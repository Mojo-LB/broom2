/* broom-report.c - "Add hardware", Windows PE side (onboard.rs). Broom boots a new PC into Windows PE with wimboot and
 * injects this program plus a startnet.cmd that runs it:
 *   broom-report.exe <server> <token> [<ip> <mask> <gateway>]
 * It lists the PC's network cards the way Windows sees them (pnputil: device instance IDs, hardware IDs, compatible
 * IDs; wmic: which card has which MAC), sends that to Broom, shows Broom's progress while Broom installs the card(s)
 * into the golden, then restarts the PC (it boots the golden's new version).
 * A native program on purpose: the Windows PE of a Windows 11 24H2 installer has cscript but no working script engine
 * (seen 31 Oct 2026: "Can't find script engine JScript/VBScript"), and no PowerShell or curl. Only kernel32 + winhttp.
 * Build: winpe/build.sh (mingw-w64), output scripts/broom-report.exe (committed, embedded as an asset). ASCII only. */
#include <windows.h>
#include <winhttp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static wchar_t host[256];
static INTERNET_PORT port = 80;
static char token[80];

static void say(const char *m) { printf("  %s\n", m); fflush(stdout); }

/* Run a command; its output (UTF-16 output from wmic folded to ASCII). Caller frees. */
static char *run(const char *cmd) {
    FILE *p = _popen(cmd, "rb");
    size_t cap = 1 << 16, n = 0;
    char *b = malloc(cap + 1);
    if (!b) return NULL;
    if (p) {
        size_t k;
        while ((k = fread(b + n, 1, cap - n, p)) > 0) {
            n += k;
            if (n == cap) {
                char *nb = realloc(b, cap * 2 + 1);
                if (!nb) break;
                b = nb;
                cap *= 2;
            }
        }
        _pclose(p);
    }
    /* UTF-16LE (wmic into a pipe) -> drop the zero bytes; then cut anything not printable ASCII. */
    size_t w = 0;
    for (size_t i = 0; i < n; i++) {
        unsigned char c = (unsigned char)b[i];
        if (c == 0 || c == 0xFF || c == 0xFE) continue;
        b[w++] = (c == '\r' || c == '\n' || c == '\t' || (c >= 32 && c < 127)) ? (char)c : '?';
    }
    b[w] = 0;
    return b;
}

/* HTTP to Broom: path is "/api/onboard/..."; the token is appended. Returns the status (0 = no answer); body in *out. */
static int http(const wchar_t *verb, const char *path, const char *body, char **out) {
    int status = 0;
    *out = NULL;
    wchar_t wpath[512];
    swprintf(wpath, 512, L"%hs?t=%hs", path, token);
    HINTERNET s = WinHttpOpen(L"broom-report", WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_NO_PROXY_NAME,
                              WINHTTP_NO_PROXY_BYPASS, 0);
    if (!s) return 0;
    WinHttpSetTimeouts(s, 5000, 5000, 30000, 30000);
    HINTERNET c = WinHttpConnect(s, host, port, 0);
    HINTERNET r = c ? WinHttpOpenRequest(c, verb, wpath, NULL, WINHTTP_NO_REFERER, WINHTTP_DEFAULT_ACCEPT_TYPES, 0) : NULL;
    DWORD len = body ? (DWORD)strlen(body) : 0;
    if (r && WinHttpSendRequest(r, L"Content-Type: text/plain\r\n", (DWORD)-1L, (LPVOID)body, len, len, 0) &&
        WinHttpReceiveResponse(r, NULL)) {
        DWORD code = 0, sz = sizeof(code);
        WinHttpQueryHeaders(r, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, NULL, &code, &sz, NULL);
        status = (int)code;
        size_t cap = 4096, n = 0;
        char *b = malloc(cap + 1);
        DWORD got = 0;
        while (b && WinHttpReadData(r, b + n, (DWORD)(cap - n), &got) && got > 0) {
            n += got;
            if (n == cap) {
                char *nb = realloc(b, cap * 2 + 1);
                if (!nb) break;
                b = nb;
                cap *= 2;
            }
        }
        if (b) b[n] = 0;
        *out = b;
    }
    if (r) WinHttpCloseHandle(r);
    if (c) WinHttpCloseHandle(c);
    WinHttpCloseHandle(s);
    return status;
}

/* "key":"value" out of Broom's JSON answer (compact, no nesting) -> buf. */
static void field(const char *json, const char *key, char *buf, size_t cap) {
    char pat[64];
    buf[0] = 0;
    if (!json) return;
    snprintf(pat, sizeof pat, "\"%s\":\"", key);
    const char *p = strstr(json, pat);
    if (!p) return;
    p += strlen(pat);
    size_t n = 0;
    while (*p && *p != '"' && n + 1 < cap) {
        if (*p == '\\' && p[1]) p++;
        buf[n++] = *p++;
    }
    buf[n] = 0;
}

static int reachable(void) {
    char *b = NULL;
    int st = http(L"GET", "/api/onboard/state", NULL, &b);
    free(b);
    return st == 200;
}

static int ipv4(const char *s) {
    int a, b, c, d;
    char x;
    return s && sscanf(s, "%d.%d.%d.%d%c", &a, &b, &c, &d, &x) == 4;
}

int main(int argc, char **argv) {
    if (argc < 3 || strncmp(argv[1], "http://", 7) != 0) {
        say("usage: broom-report.exe http://<server> <token> [<ip> <mask> <gateway>]");
        return 2;
    }
    /* http://host[:port] */
    char h[256];
    snprintf(h, sizeof h, "%s", argv[1] + 7);
    char *colon = strchr(h, ':');
    if (colon) {
        *colon = 0;
        port = (INTERNET_PORT)atoi(colon + 1);
    }
    swprintf(host, 256, L"%hs", h);
    snprintf(token, sizeof token, "%s", argv[2]);

    say("Broom: adding this PC's network card to the golden");
    /* 1. The network: Windows PE's DHCP; without a lease after ~40 s, the address iPXE had. */
    int ok = 0;
    for (int i = 0; i < 20 && !(ok = reachable()); i++) Sleep(2000);
    if (!ok && argc >= 5 && ipv4(argv[3]) && ipv4(argv[4])) {
        char cmd[512];
        printf("  no DHCP answer - using %s/%s\n", argv[3], argv[4]);
        snprintf(cmd, sizeof cmd, "wmic nicconfig where \"IPEnabled=TRUE\" call EnableStatic (\"%s\"),(\"%s\") >nul 2>&1",
                 argv[3], argv[4]);
        system(cmd);
        if (argc >= 6 && ipv4(argv[5]) && strcmp(argv[5], "0.0.0.0") != 0) {
            snprintf(cmd, sizeof cmd, "wmic nicconfig where \"IPEnabled=TRUE\" call SetGateways (\"%s\"),(1) >nul 2>&1",
                     argv[5]);
            system(cmd);
        }
        for (int i = 0; i < 15 && !(ok = reachable()); i++) Sleep(2000);
    }
    if (!ok) {
        printf("  Broom (%s) cannot be reached: Windows PE may have no driver for this network card.\n", argv[1]);
        system("ipconfig");
        return 1;
    }
    /* 2. What Windows sees: every network device with its IDs, then which one has which MAC. */
    char *pnp = run("pnputil /enum-devices /class Net /ids");
    if (pnp && !strstr(pnp, "Hardware IDs")) {
        char *more = run("pnputil /enum-devices /class Net /properties");
        if (more) {
            char *both = malloc(strlen(pnp) + strlen(more) + 3);
            if (both) {
                sprintf(both, "%s\r\n%s", pnp, more);
                free(pnp);
                pnp = both;
            }
            free(more);
        }
    }
    char *macs = run("wmic nic get MACAddress,PNPDeviceID 2>nul");
    /* Whether the firmware booted with Secure Boot on (shown in the job's log). */
    char *sb = run("reg query HKLM\\SYSTEM\\CurrentControlSet\\Control\\SecureBoot\\State /v UEFISecureBootEnabled 2>nul");
    size_t len = (pnp ? strlen(pnp) : 0) + (macs ? strlen(macs) : 0) + (sb ? strlen(sb) : 0) + 96;
    char *report = malloc(len);
    if (!report) return 1;
    snprintf(report, len, "%s\r\n=== wmic nic ===\r\n%s\r\n=== secure boot ===\r\n%s", pnp ? pnp : "", macs ? macs : "",
             sb ? sb : "");
    char *ans = NULL;
    int st = http(L"POST", "/api/onboard/report", report, &ans);
    if (st != 200) {
        printf("  Broom refused the report: %d %s\n", st, ans ? ans : "");
        printf("%s\n", report);
        return 1;
    }
    printf("  Sent. %s\n", ans ? ans : "");
    free(ans);
    /* 3. Broom installs the card(s) into the golden in a VM on the server (a few minutes). Its progress: */
    char last[1024] = "", state[64], msg[1024];
    for (;;) {
        Sleep(5000);
        ans = NULL;
        if (http(L"GET", "/api/onboard/state", NULL, &ans) != 200) {
            free(ans);
            continue;
        }
        field(ans, "state", state, sizeof state);
        field(ans, "msg", msg, sizeof msg);
        free(ans);
        for (char *c = msg; *c; c++)
            if ((unsigned char)*c >= 127 || (unsigned char)*c < 32) *c = '-';
        if (strcmp(msg, last) != 0) {
            say(msg);
            snprintf(last, sizeof last, "%s", msg);
        }
        if (strcmp(state, "done") == 0) {
            say("Done - restarting into the golden.");
            Sleep(5000);
            system("wpeutil reboot");
            return 0;
        }
        if (strcmp(state, "waiting") != 0 && strcmp(state, "installing") != 0) {
            printf("  Not done (%s) - see the Diskless page. Restarting in 60 s.\n", state[0] ? state : "no job");
            Sleep(60000);
            system("wpeutil reboot");
            return 1;
        }
    }
}
