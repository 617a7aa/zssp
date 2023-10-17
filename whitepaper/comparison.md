
## Comparison With A Few Other Protocols

*Note that ZSSP can be used in two modes: persistent and opportunistic. Persistent mode persists the key ratcheting state of sessions while opportunistic mode will automatically reset if key ratcheting information is lost. The latter is designed for cases where persistent storage is unavailable or unreliable or when the user wishes to prioritize unattended reliability over the additional security provided by persistent mode.*

| | Persistent ZSSP | Opportunistic ZSSP| WireGuard | ZeroTier Legacy Transport |
| --- | --- | --- | --- | --- |
|**Construction**|Noise\_XKhfs+psk2|Noise\_XKhfs+psk2|Noise\_IKpsk2|Static Diffie-Helman|
|**Perfect Forward Secrecy**|Yes|Yes|Yes|No|
|**Forward Secret Identity Hiding**|Yes|Yes|No|No|
|**Quantum Forward Secret**|Yes|Yes|No|No|
|**Ratcheted Forward Secrecy**|Yes|Yes|No|No|
|**Silence is a Virtue**|Yes|No|Yes|No|
|**Key-Compromise Impersonation**|Resistant|Resistant|Resistant|Vulnerable|
|**Compromise-and-Impersonate**|Resistant|Detectable|Vulnerable|Vulnerable|
|**Single Key-Compromise MitM**|Resistant|Resistant|Resistant|Vulnerable|
|**Double Key-Compromise MitM**|Resistant|Detectable|Vulnerable|Vulnerable|
|**DOS Mitigation**|Yes|Yes|Yes|No|
|**Supports Fragmentation**|Yes|Yes|No|Yes|
|**FIPS Compliant**|Yes|Yes|No|No|
|**Small Code Footprint**|Yes|Yes|Yes|No|
|**RTT**|2|2|1|Stateless|

## Definitions

* **Construction**: The mathematical construction the protocol is based upon.
* **Perfect Forward Secrecy**: An attacker with the static private keys of both party cannot decrypt recordings of messages sent between those parties.
* **Forward Secret Identity Hiding**: An attacker with the static private key of one or more parties cannot determine the identity of everyone they have previously communicated with.
* **Quantum Forward Secret**: A quantum computer powerful enough to break Elliptic-curve cryptography is not sufficient in order to decrypt recordings of messages sent between parties.
* **Ratcheted Forward Secrecy**: In order to break forward secrecy an attacker must record and break every single key exchange two parties perform, in order, starting from the first time they began communicating. Improves secrecy under weak or compromised RNG.
* **Silence is a Virtue**: A server running the protocol can be configured in such a way that it will not respond to an unauthenticated, anonymous or replayed message.
* **Key-Compromise Impersonation**: The attacker has a memory image of a single party, and attempts to create a brand new session with that party, pretending to be someone else.
* **Compromise-and-Impersonate**: The attacker has a memory image of a single party, and attempts to impersonate them on a brand new session with the other party.
* **Single Key-Compromise MitM**: The attacker has a memory image of a single party, and attempts to become a Man-in-the-Middle between them and any other party.
* **Double Key-Compromise MitM**: The attacker has a memory image of both parties, and attempts to become a Man-in-the-Middle between them.
* **Supports Fragmentation**: Transmission data can be fragmented into smaller units to support small physical MTUs.
* **FIPS Compliant**: The cryptographic algorithms used are compliant with NIST/FIPS-140 requirements.
* **CSfC**: The cryptographic algorithms used are compliant with the [NSA Commercial Solutions for Classified (CSfC)](https://www.nsa.gov/Resources/Commercial-Solutions-for-Classified-Program/) program.
* **Small Code Footprint**: The code implementing the protocol is separate from other concerns, is concise, and is therefore easy to audit.
* **RTT**: "Round-Trip-Time" - How many round trips from initiator to responder it takes to establish a session.
