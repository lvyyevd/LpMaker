import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const {
  default: Client,
  CommitmentLevel,
} = require("@triton-one/yellowstone-grpc");
const empty = () => ({
  accounts: {},
  slots: {},
  transactions: {},
  transactionsStatus: {},
  blocks: {},
  blocksMeta: {},
  entry: {},
  accountsDataSlice: [],
});
// gRPC 仅提供变化通知，交易前总是通过 confirmed RPC 重新核对整个账户快照。
// 因此断线恢复不依赖不完整的增量重放，也不会把 processed 写入当成资金到账。
export class GrpcMonitor {
  constructor(endpoint, token, pool, idleSeconds = 300, testOptions = {}) {
    this.options = {
      Client,
      now: Date.now,
      heartbeatMs: 20000,
      reconnectMs: 1000,
      messageTimeoutMs: 60000,
      ...testOptions,
    };
    this.endpoint = endpoint;
    this.token = token;
    this.pool = pool;
    this.idleMs = idleSeconds * 1000;
    this.state = {
      connected: false,
      generation: 0,
      last_message_ms: 0,
      last_data_ms: 0,
      slot: 0,
    };
    this.stopped = false;
    this.stream = null;
  }
  start() {
    this.task = this.loop();
  }
  async loop() {
    let backoff = this.options.reconnectMs;
    while (!this.stopped) {
      let timer;
      try {
        const client = new this.options.Client(
          this.endpoint,
          this.token,
          {
            grpcConnectTimeout: 15000,
            grpcHttp2KeepAliveInterval: 20000,
            grpcKeepAliveTimeout: 10000,
          },
          { enabled: false },
        );
        const connect = client.connect();
        await Promise.race([
          connect,
          new Promise((_, reject) => {
            timer = setTimeout(
              () => reject(new Error("connect timeout")),
              15000,
            );
          }),
        ]);
        clearTimeout(timer);
        if (this.stopped) break;
        const stream = await client.subscribe();
        this.stream = stream;
        const now = this.options.now();
        Object.assign(this.state, {
          connected: true,
          generation: this.state.generation + 1,
          last_message_ms: now,
          last_data_ms: 0,
          connected_at_ms: now,
        });
        const request = {
          ...empty(),
          commitment: CommitmentLevel.CONFIRMED,
          accounts: {
            pool: {
              account: [this.pool],
              owner: [],
              filters: [],
              nonemptyTxnSignature: false,
            },
          },
        };
        const ended = new Promise((resolve, reject) => {
          stream.on("error", reject);
          stream.on("end", resolve);
          stream.on("close", resolve);
          stream.on("data", (update) => {
            this.state.last_message_ms = this.options.now();
            if (update.account) {
              this.state.last_data_ms = this.options.now();
              this.state.slot = Number(update.account.slot);
              backoff = this.options.reconnectMs;
            }
            if (update.ping)
              stream.write({ ...empty(), ping: { id: 1 } }, () => {});
          });
        });
        stream.write(request, (e) => {
          if (e) stream.destroy();
        });
        timer = setInterval(() => {
          const now = this.options.now();
          if (
            now - this.state.last_message_ms > this.options.messageTimeoutMs ||
            now - (this.state.last_data_ms || this.state.connected_at_ms) >
              this.idleMs
          )
            stream.destroy();
          else
            stream.write({ ...empty(), ping: { id: 1 } }, (e) => {
              if (e) stream.destroy();
            });
        }, this.options.heartbeatMs);
        await ended;
      } catch {
        /* 不输出 SDK 错误：其错误文本可能包含端点或访问 token。 */
      } finally {
        clearInterval(timer);
        this.stream?.destroy();
        this.stream = null;
        this.state.connected = false;
      }
      if (!this.stopped)
        await new Promise((resolve) => {
          this.wake = resolve;
          this.retry = setTimeout(resolve, backoff);
        });
      backoff = Math.min(backoff * 2, 30000);
    }
  }
  async stop() {
    this.stopped = true;
    clearTimeout(this.retry);
    this.wake?.();
    this.stream?.destroy();
    await this.task;
  }
}
