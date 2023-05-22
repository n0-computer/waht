# waht protocol

## 0. Abstract

This document defines the *waht protocol*, a protocol to speak with waht servers. It uses [QUIC version 1][RFC9000] for transport and stream semantics.

## 1. Session establishment

A client initiates a waht session by opening a QUIC connection to a server endpoint, as described in [RFC9000][RFC9000]. Client and server MUST use TLS v1.3. If the server is identified by a domain name, clients MUST send the Server Name Indication ([SNI][RFC6066]). The client also MUST send an ALPN token of `n0/waht/0`.

After establishing a waht session, the waht protocol uses QUIC streams and waht frames for communication.

## 2. Streams

### 2.1 Unidirectional streams

Unidirectional streams can be opened by both client and server.

The first byte sent on a unidirectional stream defines its stream type. If a peer does not support the stream type, it MUST close the stream (TODO: Error code).

#### 2.1.1 *CONTROL* streams

Stream type : `0x00`

A *CONTROL* stream is identified by the stream type `0x00`. Both the client and the server MUST open a *CONTROL* stream right after establishing the connection and MUST accept a control stream by the other peer. If either condition is not met, the connection MUST be aborted (TODO: Error code).

### 2.2 Bidirectional streams

The waht protocol uses only bidirectional streams opened by the client.

The first byte sent on a bidirectional stream by the client defines its stream type. If a server does not support the stream type, it MUST close the stream (TODO: Error code).

#### 2.2.1 *QUERY* stream

Stream type : `0x01`

A *QUERY* stream is opened by the client with a *QUERY* frame. The server replies with zero or more *ANNOUNCE* frames. The *ANNOUNCE* frames contain the original signatures of the announcing peers, enabling end to end verification.

If the server will not send further replies, it sends an *END* frame and closes the sending side of the bidirectional stream.

If the client is no longer interested in further results, it sends an *END* frame and closes both the sending and receiving side of the stream.

Example:
```
Client -> QUERY
Server <- ANNOUNCE
Server <- ANNOUNCE
Client -> END
Server <- ANNOUNCE
Server <- END
Server <- no more results, close send side
Client -> close send & recv sides
```

#### 2.2.2 *ANNOUNCE* stream

Stream type : `0x02`

An *ANNOUNCE* stream is opened by the client. The client sends *ANNOUNCE* frames. The server replies with *ACK* frames.

Example:
```
Client -> ANNOUNCE
Client -> ANNOUNCE
Client -> ANNOUNCE
Server <- ACK
Server <- ACK
Server <- ACK
Client -> ANNOUNCE
Client -> UNANNOUNCE 
Server <- ACK
Server <- ACK
```

### 3. Frames

After the initial byte setting the stream type, all streams defined in section 2 carry *waht frames*. Most frames are only allowed on one or some stream types.

#### 3.1 Frame layout

All frames share the same layout:
```
Type (u32)
Length (u32)
Frame Payload (..)
```

### 3.2 Frame definition

#### 3.2.1 `HELLO`

Streams: `CONTROL` (client, server)

```
Hello {
    PeerId (Key)
    Capabilities (Token..)
}

Hello FRAME {
  Type (i) = 0x01
  Length (i)
  Hello (Hello)
}
```

#### 3.2.X `ANNOUNCE`

Streams: `ANNOUNCE` (client), `QUERY` (server)

```
AnnouncePayload = PeerInfo | AnnounceTarget

PeerInfo = {
    Type = 0x00
    SocketAddress (SocketAddress..)
    Relay (RelayAddress..)
}

AnnounceTarget = {
    Type = 0x01
    Target (Hash)
    Topics (Hash..)
}

Signature = {
    Algorithm = 0x00
    Signature (64)
}

Via = {
    PeerId (32)
    Signature (Signature?)
}

ANNOUNCE Frame {
  Type (1) = 0x0X
  Length (1)
  PeerId (PeerId)
  Payload (AnnouncePayload)
  Timestamp (8)
  Signature (Signature)
  Via (Via..)
}

```

The Signature is an Ed25591 signature with the PeerId as the verifying key and the bytes of the payload as the signature's payload. When receiving announce frames, the signature MUST be checked.

#### 3.2.X `UNANNOUNCE`

Streams: `ANNOUNCE` (client), `QUERY` (server)

```
UNANNOUNCE Frame {
  Type (1) = 0x0X
  Length (1)
  PeerId (PeerId)
  Payload (AnnouncePayload)
  Timestamp (8)
  Signature (Signature)
  Via (Via..)
}
```


#### 3.2.X `QUERY`

Streams: `QUERY` (client)

```
Query = QueryPeers | QueryKeys

QueryPeers = {
    Type = 0x01
    PeerIds (Key..)
}

QueryKeys = {
    Type = 0x02
    Keys (Key..)
}

Settings = {
    KeepAlive (bool)
    Federate (bool)
    StopAfter (u32)
}

QUERY Frame {
  Type (i) = 0x0X
  Length (i)
  Settings (Settings)
  Query (Query)
}
```

#### 3.2.X `CANCEL_QUERY`

Streams: `QUERY` (client)

```
CANCEL_QUERY Frame {
  Type (i) = 0x0X
  Length (i)
  Cancellations (Query)
}
```

#### 3.2.X `REDIRECT`

Streams: `CONTROL`, `QUERY`, `ANNOUNCE` (all server only)

```
REDIRECT Frame {
  Type (i) = 0x0X
  Length (i)
  Server (URL)
  Topics (Topic..)
}
```

#### 3.2.X `ACK`

Streams: `ANNOUNCE` (server)

```
ACK Frame {
  Type (i) = 0x0X
  Length (i) = 0
}
```

#### 3.2.X `END`

Streams: All

```
ACK Frame {
  Type (i) = 0x0X
  Length (1) = 0
}
```

#### 3.2.X `ERROR`

Streams: All

```
ERROR Frame {
  Type (i) = 0x0X
  Length (i)
  Code (u32)
  Message (String)
}
```
## References

[RFC6066]: https://www.rfc-editor.org/rfc/rfc6066 "Transport Layer Security (TLS) Extensions: Extension Definitions"
[RFC9000]: https://www.rfc-editor.org/rfc/rfc9000 "QUIC: A UDP-Based Multiplexed and Secure Transport (RFC9000)"
