# Security Policy

## Supported Versions

| Version | Supported |
| ------- | --------- |
| 0.1.x   | Yes       |

## Reporting a Vulnerability

If you discover a security vulnerability in Allman, **please do not open a public issue.**

Instead, report it privately:

1. Email **security@copyleftdev.com** with a description of the vulnerability, steps to reproduce, and potential impact.
2. You will receive an acknowledgment within **48 hours**.
3. We will work with you to understand the scope and coordinate a fix before any public disclosure.

## Scope

Allman is an agent mail server designed for **trusted internal networks**. The current threat model assumes:

- The server runs behind a firewall or VPN.
- There is no built-in authentication or authorization on the MCP endpoint.
- Agent identities are not cryptographically verified.

If you are exposing Allman to untrusted networks, you must provide your own authentication layer (e.g., reverse proxy with mTLS, API gateway).

## Disclosure Policy

- We follow **coordinated disclosure**. We ask reporters to allow up to 90 days for a fix before public disclosure.
- Security fixes will be released as patch versions and documented in the changelog.
- Credit will be given to reporters unless they prefer anonymity.
