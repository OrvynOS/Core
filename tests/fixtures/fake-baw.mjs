// A stand-in for Binance's `baw` CLI (tests/binance_wallet.rs): the same --json envelopes, and its
// session in $BINANCE_BAW_DIR/session.json, readable only with the BINANCE_INSTANCE_ID it was made
// with. Every call is appended to $FAKE_BAW_LOG.
import fs from "node:fs";
import path from "node:path";

const argv = process.argv.slice(2).filter((a) => a !== "--json");
const dir = process.env.BINANCE_BAW_DIR;
const instance = process.env.BINANCE_INSTANCE_ID;
const file = path.join(dir, "session.json");
fs.appendFileSync(process.env.FAKE_BAW_LOG, JSON.stringify({ argv, instance, dir }) + "\n");

const out = (data) => console.log(JSON.stringify({ success: true, data }, null, 2));
const fail = (code, name, message) => {
  console.log(JSON.stringify({ success: false, error: { code, name, message } }, null, 2));
  process.exit(1);
};
const flag = (name) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : undefined;
};
let session = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, "utf8")) : null;
if (session && session.instance !== instance) session = null; // another instance can't read it
const save = (s) => fs.writeFileSync(file, JSON.stringify(s));
const signedIn = () => session?.sessionId;
const cmd = argv.slice(0, 2).join(" ");

if (cmd === "auth signin") {
  if (signedIn()) out({ status: "ALREADY_CONNECTED" });
  else {
    save({ instance, clientId: "client-1" });
    out({ qrCodeId: process.env.FAKE_BAW_QR ?? "qr-ok", expireAt: String(Date.now() + 300000), urlForWeb: "https://app.binance.com/uni-qr/test", pairingCode: "a1b2c3" });
  }
} else if (cmd === "auth verify") {
  if (flag("qrCodeId") !== "qr-ok" || !session?.clientId) fail(10002004, "AUTH_REJECTED", "QR code does not exist or expired");
  save({ ...session, sessionId: "sess-1" });
  out({ status: "SUCCESS" });
} else if (cmd === "auth signout") {
  fs.rmSync(file, { force: true });
  out({ status: "LOGGED_OUT" });
} else if (cmd === "wallet status") {
  out({ status: signedIn() ? "CONNECTED" : "UNCONNECTED" });
} else if (!signedIn()) {
  fail(10003000, "NOT_LOGGED_IN", "Not logged in");
} else if (cmd === "wallet address") {
  out([{ binanceChainId: "56", simpleName: "BSC", address: "0x00000000000000000000000000000000000000aa" }, { binanceChainId: "CT_501", simpleName: "Solana", address: "So1aaa" }]);
} else if (cmd === "wallet settings") {
  out({ dailyLimit: 500, tradeAllTokens: false, abnormalTxnHandling: "NeedConfirmation", sessionExpireTime: "2026-12-01T00:00:00Z" });
} else if (cmd === "wallet balance") {
  out([{ symbol: "BNB", amount: "1.5" }]);
} else if (cmd === "market-order quote") {
  // 1 unit of anything is $600 in USDT.
  out({ fromCoinSymbol: "BNB", fromCoinAmount: flag("fromTokenQty"), toCoinSymbol: "USDT", toCoinAmount: String(Number(flag("fromTokenQty")) * 600), slippage: 0.005 });
} else if (cmd === "market-order swap") {
  out({ orderId: "order-1" });
} else if (cmd === "market-order list") {
  out({ total: 1, page: 1, pageSize: 20, list: [{ orderId: flag("orderId"), status: "FINISHED", txHash: "0xabc" }] });
} else if (cmd === "limit-order cancel") {
  out({ strategyId: flag("strategyId"), status: "CANCELED" });
} else if (cmd === "wallet send") {
  out({ txHash: "0xsent" });
} else if (cmd === "expire") {
  fs.rmSync(file, { force: true });
  fail(10003002, "SESSION_EXPIRED", "Session expired");
} else {
  fail(1001002, "INVALID_PARAMS", `unknown command ${cmd}`);
}
