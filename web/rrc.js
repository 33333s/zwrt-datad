// Port of UFIPanel RrcUperDecoder: bounded unaligned PER, using the same ASN.1 graph.
// Unknown extensions and truncated/unsupported layouts fail closed; opaque bytes stay hex.
const check = (condition) => {
  if (!condition) throw new Error("Invalid UPER");
};
const exact = (value) =>
  value >= BigInt(Number.MIN_SAFE_INTEGER) &&
  value <= BigInt(Number.MAX_SAFE_INTEGER)
    ? Number(value)
    : value.toString();
class Bits {
  constructor(bytes) {
    this.bytes = bytes;
    this.pos = 0;
  }
  get remaining() {
    return this.bytes.length * 8 - this.pos;
  }
  read(n) {
    check(Number.isInteger(n) && n >= 0 && n <= 63 && n <= this.remaining);
    let value = 0n;
    for (let i = 0; i < n; i++, this.pos++)
      value =
        (value << 1n) |
        BigInt((this.bytes[this.pos >> 3] >> (7 - (this.pos % 8))) & 1);
    return value;
  }
  flag() {
    return this.read(1) !== 0n;
  }
  octets(n) {
    check(n >= 0 && n <= 65535 && n <= this.remaining / 8);
    return Uint8Array.from({ length: n }, () => Number(this.read(8)));
  }
  length() {
    const a = Number(this.read(8));
    if (a < 128) return a;
    check(a < 192);
    return ((a & 63) << 8) | Number(this.read(8));
  }
  small() {
    if (!this.flag()) return Number(this.read(6));
    const n = this.length();
    check(n >= 1 && n <= 4);
    const v = Number(this.read(n * 8));
    check(v <= 65535);
    return v;
  }
  constrained(lo, hi) {
    lo = BigInt(lo);
    hi = BigInt(hi);
    check(hi >= lo);
    const range = hi - lo,
      count = range ? range.toString(2).length : 0;
    const v = this.read(count);
    check(v <= range);
    return lo + v;
  }
  finishOpen() {
    check(this.remaining <= 7 || (this.pos === 0 && this.remaining === 8));
    check(this.read(this.remaining) === 0n);
  }
}

export class RrcDecoder {
  constructor(schema) {
    this.schema = schema;
  }
  decode(radio, channel, payload) {
    if (!payload.length || payload.length > 65535) return null;
    const root = this.schema.roots[`${radio}:${channel}`];
    if (root === undefined) return null;
    try {
      const bits = new Bits(payload),
        budget = { nodes: 0 };
      const tree = this.value(root, bits, budget, 0);
      return { tree, consumedBits: bits.pos, trailingBits: bits.remaining };
    } catch {
      return null;
    }
  }
  size(n, bits) {
    const extension = n.sizeExt && bits.flag();
    const v =
      !extension && n.min !== undefined && n.max !== undefined && n.max < 65536
        ? Number(bits.constrained(n.min, n.max))
        : bits.length();
    check(v >= 0 && v <= 65535);
    return v;
  }
  open(id, bits, budget, depth) {
    const inner = new Bits(bits.octets(bits.length()));
    const v = this.value(id, inner, budget, depth + 1);
    inner.finishOpen();
    return v;
  }
  value(id, bits, budget, depth) {
    check(depth <= 160 && ++budget.nodes <= 100000);
    const n = this.schema.nodes[id];
    check(n);
    const root = n.root ?? [],
      extras = n.extra ?? [];
    switch (n.t) {
      case "SEQUENCE": {
        const extension = n.ext && bits.flag(),
          present = root.map((f) => !f[2] || bits.flag()),
          fields = Object.create(null);
        for (let i = 0; i < root.length; i++) {
          const [name, child] = root[i];
          if (present[i])
            fields[name] = this.value(child, bits, budget, depth + 1);
          else if (Object.hasOwn(this.schema.nodes[child], "default"))
            fields[name] = this.schema.nodes[child].default;
        }
        if (extension) {
          const count = bits.small() + 1;
          check(count <= 4096);
          const bitmap = Array.from({ length: count }, () => bits.flag());
          for (let i = 0; i < count; i++)
            if (bitmap[i]) {
              check(i < extras.length);
              const [name, child] = extras[i],
                value = this.open(child, bits, budget, depth);
              if (name === "") {
                check(
                  value && typeof value === "object" && !Array.isArray(value),
                );
                Object.assign(fields, value);
              } else fields[name] = value;
            }
        }
        return fields;
      }
      case "CHOICE":
      case "ENUMERATED": {
        const extension = n.ext && bits.flag(),
          list = extension ? extras : root;
        const index = extension
          ? bits.small()
          : Number(bits.constrained(0, list.length - 1));
        check(index >= 0 && index < list.length);
        if (n.t === "ENUMERATED") return list[index];
        const [name, child] = list[index];
        return {
          [name]: extension
            ? this.open(child, bits, budget, depth)
            : this.value(child, bits, budget, depth + 1),
        };
      }
      case "SEQUENCE OF": {
        const count = this.size(n, bits);
        return Array.from({ length: count }, () =>
          this.value(n.item, bits, budget, depth + 1),
        );
      }
      case "INTEGER": {
        const extension = n.sizeExt && bits.flag();
        if (!extension && n.min !== undefined && n.max !== undefined)
          return exact(bits.constrained(n.min, n.max));
        const count = bits.length();
        check(count >= 1 && count <= 16);
        const bytes = bits.octets(count);
        let v = 0n;
        for (const b of bytes) v = (v << 8n) | BigInt(b);
        if (!extension && n.min !== undefined) v += BigInt(n.min);
        else if (bytes[0] & 128) v -= 1n << BigInt(count * 8);
        return exact(v);
      }
      case "BOOLEAN":
        return bits.flag();
      case "NULL":
        return null;
      case "OCTET STRING":
      case "BIT STRING": {
        const count = this.size(n, bits),
          length = count * (n.t === "BIT STRING" ? 1 : 8);
        check(length <= bits.remaining);
        const bytes = new Uint8Array(Math.ceil(length / 8));
        for (let i = 0; i < length; i++)
          if (bits.flag()) bytes[i >> 3] |= 128 >> i % 8;
        if (n.contained === undefined)
          return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join(
            "",
          );
        const inner = new Bits(bytes),
          value = this.value(n.contained, inner, budget, depth + 1);
        inner.finishOpen();
        return { [n.containedName]: value };
      }
      default:
        throw new Error("Unsupported ASN.1 node");
    }
  }
}
