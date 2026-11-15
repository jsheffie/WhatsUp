// Route every request of this Chrome profile through WhatsUp's SSH SOCKS
// tunnel, so the remote machine resolves host names and connects, exactly as
// a browser running there would. Keep the port in sync with WhatsUp's
// DEFAULT_SOCKS_PORT (src/config.rs).
const SOCKS_PORT = 47655;

function applyProxy() {
  chrome.proxy.settings.set({
    scope: "regular",
    value: {
      mode: "fixed_servers",
      rules: {
        singleProxy: { scheme: "socks5", host: "127.0.0.1", port: SOCKS_PORT },
        // Chrome never proxies localhost unless told to; links like
        // http://localhost:4004 must reach the remote machine too.
        bypassList: ["<-loopback>"],
      },
    },
  });
}

chrome.runtime.onInstalled.addListener(applyProxy);
chrome.runtime.onStartup.addListener(applyProxy);
