# Hext Remote

Acesso e suporte remoto para Windows, macOS e Linux, focado em fluidez.

- Plano completo: [ARCHITECTURE.md](ARCHITECTURE.md)
- Fase atual: **F0 — Fundação** (workspace, CI e esqueleto do protocolo)

## Estrutura

| Pasta | Conteúdo |
|---|---|
| `crates/proto` | Mensagens do protocolo, versão e framing |
| `crates/net` | QUIC, verificação do host por impressão digital, handshake e datagramas |
| `crates/metrics` | Estatísticas de latência (base do overlay de métricas) |
| `apps/hext` | O app. Por enquanto é uma linha de comando que testa a conexão entre duas máquinas. |

## Desenvolvimento

Precisa do [Rust](https://rustup.rs). A versão é fixada em `rust-toolchain.toml` e instalada sozinha.

```sh
cargo test --workspace     # testes
cargo run -p hext -- --help
```

## Baixar os executáveis do CI

Cada push gera executáveis em **GitHub → Actions → CI → (execução) → Artifacts**:

- `HextRemote-windows-x64` → `HextRemote.exe`
- `HextRemote-macos-universal` → `HextRemote` (Intel e Apple Silicon)
- `hext-remote-linux-x64` → `hext-remote`

Os binários ainda não são assinados (isso entra na F6):

- **Windows:** o SmartScreen pode avisar. Clique em "Mais informações" → "Executar assim mesmo".
- **macOS:** antes de rodar, no Terminal, na pasta do arquivo:

  ```sh
  chmod +x HextRemote && xattr -d com.apple.quarantine HextRemote
  ```

## Testar entre dois computadores (mesma rede)

1. No computador que será acessado:

   ```sh
   HextRemote.exe host          # Windows
   ./HextRemote host            # macOS
   ```

   Ele mostra uma **impressão digital**. No Windows, permita o acesso quando o Firewall perguntar (porta UDP 7420).

2. No outro computador, com o IP do primeiro:

   ```sh
   ./HextRemote connect 192.168.0.10 --fingerprint <impressão digital>
   ```

   Ele mostra o nome e o sistema do host, a latência (mín, média, p95, máx) e os pacotes perdidos.
