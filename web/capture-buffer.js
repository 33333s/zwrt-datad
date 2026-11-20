// Amortized O(1) retention; large raw captures must not shift a 100k-element array per packet.
export class CaptureBuffer {
  constructor(limit = 24 * 1024 * 1024) {
    this.limit = limit;
    this.clear();
  }
  clear() {
    this.items = [];
    this.head = 0;
    this.bytes = 0;
  }
  push(packet, observedAt) {
    const size = packet.byteLength + 4;
    this.items.push({ packet, observedAt });
    this.bytes += size;
    while (this.bytes > this.limit && this.items.length - this.head > 1)
      this.bytes -= this.items[this.head++].packet.byteLength + 4;
    if (this.head > 4096 && this.head * 2 > this.items.length) {
      this.items = this.items.slice(this.head);
      this.head = 0;
    }
  }
  get count() {
    return this.items.length - this.head;
  }
  snapshot() {
    return this.items.slice(this.head);
  }
}
