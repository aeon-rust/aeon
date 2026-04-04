/**
 * Aeon event and output types for AssemblyScript processors.
 */

/** A key-value metadata pair. */
export class Header {
  constructor(
    public key: string,
    public value: string,
  ) {}
}

/** An event received from the Aeon pipeline. */
export class Event {
  constructor(
    /** Event ID as raw UUID bytes (16 bytes). */
    public id: Uint8Array,
    /** Unix epoch nanoseconds. */
    public timestamp: i64,
    /** Source identifier. */
    public source: string,
    /** Partition number. */
    public partition: u16,
    /** Key-value metadata headers. */
    public metadata: Header[],
    /** Event payload bytes. */
    public payload: Uint8Array,
  ) {}

  /** Get the payload as a UTF-8 string. */
  payloadString(): string {
    return String.UTF8.decode(this.payload.buffer);
  }
}

/** An output to emit from the processor. */
export class Output {
  public key: Uint8Array | null;
  public headers: Header[];

  constructor(
    /** Destination sink/topic name. */
    public destination: string,
    /** Output payload bytes. */
    public payload: Uint8Array,
  ) {
    this.key = null;
    this.headers = [];
  }

  /** Create an output from a string payload. */
  static fromString(destination: string, payload: string): Output {
    const buf = String.UTF8.encode(payload);
    return new Output(destination, Uint8Array.wrap(buf));
  }

  /** Set the partition key. */
  withKey(key: Uint8Array): Output {
    this.key = key;
    return this;
  }

  /** Set the partition key from a string. */
  withKeyString(key: string): Output {
    this.key = Uint8Array.wrap(String.UTF8.encode(key));
    return this;
  }

  /** Add a header. */
  withHeader(key: string, value: string): Output {
    this.headers.push(new Header(key, value));
    return this;
  }
}
