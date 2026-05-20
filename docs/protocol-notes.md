# Protocol Implementation Notes

Reference for engineers reading or modifying this codebase. This is **not** a
substitute for the IEC 60870-5 standards documents (-1 transmission frame formats,
-2 link transmission procedures, -4 application information elements,
-5 application functions, -101 companion standard for serial, -104 companion
standard for TCP/IP). When in doubt the standard is authoritative; this document
captures byte layouts, defaults, and design decisions in one place so future
contributors don't have to rediscover them.

## IEC 60870-5-101 — FT 1.2 framing

Per IEC 60870-5-1 / -2 there are three frame formats:

### Single-character frame

A single octet acknowledging service requests.

| Octet | Value | Meaning |
| --- | --- | --- |
| 0 | `0xE5` | Positive ACK |
| 0 | `0xA2` | Negative ACK (less common; used by some companion standards) |

### Fixed-length frame (5 octets)

Used for link-layer control without payload.

```
+------+----+----+----+------+
| 0x10 | C  | LA | CS | 0x16 |
+------+----+----+----+------+
```

| Offset | Field | Notes |
| --- | --- | --- |
| 0 | Start | `0x10` |
| 1 | Control | Function code + PRM/FCB/FCV/DFC bits |
| 2 | Link address | 1 or 2 octets depending on configuration |
| n-2 | Checksum (CS) | Arithmetic sum of C..LA (last octet), modulo 256 |
| n-1 | End | `0x16` |

> **Important.** The IEC 60870-5-1 checksum is **not** CRC-16; it is the
> arithmetic sum of the user octets taken as unsigned bytes, modulo 256.

### Variable-length frame

Used for application data (ASDU payload).

```
+------+----+----+------+----+----+--------+----+------+
| 0x68 | L  | L  | 0x68 | C  | LA | ASDU.. | CS | 0x16 |
+------+----+----+------+----+----+--------+----+------+
```

| Offset | Field |
| --- | --- |
| 0 | Start = `0x68` |
| 1 | Length L (count of C + LA + ASDU octets) |
| 2 | Length L (repeated for redundancy; must match) |
| 3 | Start = `0x68` (repeated) |
| 4 | Control field |
| 5 | Link address (1 or 2 octets) |
| 6..6+L−2 | ASDU |
| 6+L | Checksum (sum mod 256 over C + LA + ASDU) |
| 7+L | End = `0x16` |

### Control field (Link layer)

Bit layout (octet, MSB → LSB):

```
| 7    | 6  | 5      | 4      | 3..0     |
| RES  | PRM| FCB/ACD| FCV/DFC| FuncCode |
```

* `PRM` — primary message (1 = master initiates, 0 = secondary)
* `FCB` — frame count bit (alternates per send/confirm; primary only)
* `FCV` — frame count valid; if 0, FCB is ignored
* In secondary direction: bit 5 = ACD (access demand), bit 4 = DFC (data flow control)

Function codes (primary direction):

| Code | Meaning |
| --- | --- |
| 0 | Reset of remote link |
| 1 | Reset of user process |
| 3 | User data, confirmed (SEND/CONFIRM) |
| 4 | User data, unconfirmed (SEND/NO REPLY) |
| 9 | Request status of link |
| 10 | Request user data class 1 (REQ/RESP) |
| 11 | Request user data class 2 (REQ/RESP) |

Function codes (secondary direction):

| Code | Meaning |
| --- | --- |
| 0 | ACK |
| 1 | NACK |
| 8 | Response with user data |
| 9 | NACK (no user data) |
| 11 | Status of link |

## IEC 60870-5-104 — APCI / APDU

All multi-octet integers in the APCI are **little-endian**.

### Common APCI header (6 octets)

```
+------+------+----+----+----+----+
| 0x68 |  L   | C1 | C2 | C3 | C4 |
+------+------+----+----+----+----+
```

* `L` is the count of the following octets (4 control + ASDU), so total APDU
  length on the wire is `L + 2`. Max `L = 253` → max APDU 255 octets.

Three frame formats are distinguished by the low bits of C1:

| C1 bits | Format | Purpose |
| --- | --- | --- |
| `....0`  | I (Information transfer) | Carries an ASDU; numbered with N(S), N(R) |
| `..01`  | S (Supervisory) | Acknowledges received I-frames |
| `..11`  | U (Unnumbered) | Connection control (STARTDT, STOPDT, TESTFR) |

### I-format

```
C1 = NS_lo << 1
C2 = NS_hi
C3 = NR_lo << 1
C4 = NR_hi
```

* N(S) and N(R) are 15-bit sequence numbers stored in the upper 15 bits of a
  little-endian u16. Wire layout: `(NS << 1)` little-endian, then `(NR << 1)`
  little-endian. The low bit of C1 must be 0 (I-format discriminator); the low
  bit of C3 must be 0 (always).
* Cycle modulo 2^15 = 32768.

### S-format

```
C1 = 0x01
C2 = 0x00
C3 = NR_lo << 1
C4 = NR_hi
```

### U-format

```
C1 = 0b<flag6><flag5><flag4><flag3><flag2><flag1>11
C2 = 0x00
C3 = 0x00
C4 = 0x00
```

Where `flag1..flag6` are pairs of `act` and `con` bits for each control function:

| Bits in C1 | Function |
| --- | --- |
| 0x07 (bit 2) | STARTDT act |
| 0x0B (bit 3) | STARTDT con |
| 0x13 (bit 4) | STOPDT act |
| 0x23 (bit 5) | STOPDT con |
| 0x43 (bit 6) | TESTFR act |
| 0x83 (bit 7) | TESTFR con |

Exactly one of `act`/`con` per pair is set in any valid U-frame.

### Connection state machine (per IEC 60870-5-104 §5)

States:

* **Idle** — TCP not connected
* **Connecting** — TCP connecting, awaiting STARTDT or sending it
* **Started** — data transfer enabled; I-frames may flow
* **Stopping** — STOPDT initiated, waiting for outstanding ACKs
* **Closed** — connection terminated

Timers (defaults per IEC 60870-5-104 §9.6, all in seconds, configurable):

| Timer | Default | Range | Purpose |
| --- | --- | --- | --- |
| t0 | 30 | 1..255 | TCP connection establishment timeout |
| t1 | 15 | 1..255 | Timeout of send / test APDU (no ACK by t1 → close) |
| t2 | 10 | 1..255 | Acknowledge timeout when no I-frames in t2 of last receive |
| t3 | 20 | 1..172800 | Idle TESTFR; send TESTFR act after t3 idle |

Constraints: `t2 < t1` and `t3 > t1`.

Flow control parameters:

| Param | Default | Range | Purpose |
| --- | --- | --- | --- |
| k | 12 | 1..32767 | Max unacknowledged I-APDUs sent before backpressure |
| w | 8  | 1..32767 | Receive must ACK by w I-APDUs (or t2) |

Recommendation: `w ≤ 2/3 · k`.

## ASDU (shared between 101 and 104)

```
+-------------+------+------+--------+-------------+
| Type ID (1) | VSQ  | COT  |  CA    | InfoObjects |
+-------------+------+------+--------+-------------+
```

* **Type ID** (1 octet) — identifies the ASDU type (see table below).
* **VSQ** (1 octet) — variable structure qualifier.
  * bit 7 = SQ (sequence of objects: 0 = each IO has its own IOA, 1 = single
    starting IOA and consecutive)
  * bits 0..6 = number of information objects (0..127)
* **COT** (1 or 2 octets) — cause of transmission. Optional originator address
  in second byte; configured per system.
* **CA** (1 or 2 octets) — common address of ASDU. For IEC 60870-5-104, CA is
  fixed at **2 octets**.
* **Information objects** — each begins with an IOA (1, 2, or 3 octets;
  always 3 octets for 104). Followed by information elements specific to the
  type ID.

### ASDU types we plan to support in 0.1.0

Monitor direction (`M_*`):

| ID | Mnemonic | Description |
| --- | --- | --- |
| 1 | M_SP_NA_1 | Single-point information |
| 3 | M_DP_NA_1 | Double-point information |
| 5 | M_ST_NA_1 | Step position information |
| 7 | M_BO_NA_1 | Bitstring of 32 bit |
| 9 | M_ME_NA_1 | Measured value, normalized |
| 11 | M_ME_NB_1 | Measured value, scaled |
| 13 | M_ME_NC_1 | Measured value, short floating point |
| 15 | M_IT_NA_1 | Integrated totals |
| 30 | M_SP_TB_1 | Single-point with CP56Time2a |
| 31 | M_DP_TB_1 | Double-point with CP56Time2a |
| 34 | M_ME_TD_1 | Normalized + CP56Time2a |
| 35 | M_ME_TE_1 | Scaled + CP56Time2a |
| 36 | M_ME_TF_1 | Float + CP56Time2a |

Control direction (`C_*`):

| ID | Mnemonic | Description |
| --- | --- | --- |
| 45 | C_SC_NA_1 | Single command |
| 46 | C_DC_NA_1 | Double command |
| 47 | C_RC_NA_1 | Regulating step command |
| 48 | C_SE_NA_1 | Set-point, normalized |
| 49 | C_SE_NB_1 | Set-point, scaled |
| 50 | C_SE_NC_1 | Set-point, short floating point |
| 58 | C_SC_TA_1 | Single command + CP56Time2a |
| 61 | C_SE_TA_1 | Set-point normalized + CP56Time2a |
| 62 | C_SE_TB_1 | Set-point scaled + CP56Time2a |
| 63 | C_SE_TC_1 | Set-point float + CP56Time2a |

System information / process:

| ID | Mnemonic | Description |
| --- | --- | --- |
| 70 | M_EI_NA_1 | End of initialisation |
| 100 | C_IC_NA_1 | Interrogation command |
| 101 | C_CI_NA_1 | Counter interrogation |
| 102 | C_RD_NA_1 | Read command |
| 103 | C_CS_NA_1 | Clock synchronisation |
| 104 | C_TS_NA_1 | Test command (legacy) |
| 105 | C_RP_NA_1 | Reset process command |
| 107 | C_TS_TA_1 | Test command + CP56Time2a |

### Cause of Transmission (selected)

| Code | Mnemonic | Meaning |
| --- | --- | --- |
| 1 | per/cyc | Periodic, cyclic |
| 2 | back | Background scan |
| 3 | spont | Spontaneous |
| 4 | init | Initialised |
| 5 | req | Request or requested |
| 6 | act | Activation |
| 7 | actcon | Activation confirmation |
| 8 | deact | Deactivation |
| 9 | deactcon | Deactivation confirmation |
| 10 | actterm | Activation termination |
| 20 | inrogen | Interrogated by station |
| 44 | unknown_type | Type-ID unknown |
| 45 | unknown_cause | COT unknown |
| 46 | unknown_addr | CA unknown |
| 47 | unknown_info | IOA unknown |

Bits 6 (P/N negative) and 7 (T test) modify the COT.

### Information elements (IEC 60870-5-4)

Frequently used:

| IE | Octets | Description |
| --- | --- | --- |
| SIQ | 1 | Single-point info with quality |
| DIQ | 1 | Double-point info with quality |
| QDS | 1 | Quality descriptor |
| NVA | 2 | Normalized value (i16 mapped to −1..+1−2⁻¹⁵) |
| SVA | 2 | Scaled value (i16) |
| R32 | 4 | IEEE 754 binary32 |
| BCR | 5 | Binary counter reading |
| CP24Time2a | 3 | Time tag, minutes precision |
| CP56Time2a | 7 | Time tag, ms precision, full date |

## Design decisions (cross-cutting)

* **Endianness in code** — always use `bytes::Buf::get_u16_le`,
  `bytes::BufMut::put_u16_le` etc. Never platform-default.
* **Sequence numbers** — implement `SeqNo(u16)` newtype with `wrapping_add` and
  modular comparison (≤ in the 15-bit ring).
* **State machines** — sans-I/O. Inputs: parsed APDU, user actions, `tick(now)`.
  Outputs: `Vec<Action>` consumed by the I/O layer.
* **ASDU registry** — `trait AsduPayload { const TYPE_ID: u8; fn encode/decode }`.
  A wrapper enum holds all built-in types and a `Raw { type_id, payload }`
  variant preserves unknown ASDUs rather than rejecting frames.
* **Hooks** — `EventHandler` trait with default no-op methods; library types are
  generic over `H: EventHandler` so the default is monomorphised away.

## References

* IEC 60870-5-101:2003 + A1:2015 — Companion standard for basic telecontrol tasks
* IEC 60870-5-104:2006 + A1:2016 — Network access using TCP/IP
* IEC 60870-5-4:1993 — Definition and coding of application information elements
* IEC 62351-3 — Profiles including TLS over TCP/IP
* lib60870-C — https://github.com/mz-automation/lib60870 (BSD-licensed reference impl)
