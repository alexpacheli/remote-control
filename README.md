# Hext Remote

Acesso e suporte remoto para Windows, macOS e Linux, focado em fluidez.

- Plano completo: [ARCHITECTURE.md](ARCHITECTURE.md)
- Fase atual: **F2 — Mac como host** (host Windows e macOS; visualizador em Windows, macOS e Linux)

## Estrutura

| Pasta | Conteúdo |
|---|---|
| `crates/proto` | Mensagens do protocolo: handshake, vídeo, input |
| `crates/net` | QUIC, verificação do host por impressão digital, streams |
| `crates/capture` | Captura de tela (DXGI no Windows, ScreenCaptureKit no macOS) e imagem de teste |
| `crates/codec` | H.264: placa de vídeo (Media Foundation no Windows, VideoToolbox no Mac) ou OpenH264 em software |
| `crates/input` | Mapa de teclas e injeção de mouse/teclado (SendInput no Windows, CGEvent no macOS) |
| `crates/render` | Renderização na GPU (wgpu), YUV → RGB no shader |
| `crates/session` | Pipelines do host e do cliente |
| `crates/metrics` | Estatísticas de latência |
| `apps/hext` | O app: comandos `host` e `view` |

## Desenvolvimento

Precisa do [Rust](https://rustup.rs). A versão é fixada em `rust-toolchain.toml` e instalada sozinha.

```sh
cargo test --workspace     # testes (inclui um teste de ponta a ponta host → cliente)
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

## Testar: controlar o Windows a partir do Mac (mesma rede)

1. **No Windows** (o computador que será acessado), no PowerShell, na pasta do arquivo:

   ```sh
   .\HextRemote.exe host
   ```

   Ele mostra o **endereço**, um **código de acesso** e a **impressão digital**, além do comando
   pronto para o outro computador. Permita o acesso quando o Firewall perguntar (porta UDP 7420).

2. **No Mac**, cole o comando mostrado pelo Windows, trocando o nome do programa:

   ```sh
   ./HextRemote view 192.168.0.10 --code 482913557 --fingerprint <impressão digital>
   ```

   Abre uma janela com a tela do Windows. Mouse e teclado da janela controlam o Windows.
   O título da janela mostra fps, banda, tempos de captura/encode/decode e a latência (RTT).

- **Tecla Command:** por padrão funciona como Ctrl (Cmd+C copia no Windows). Use `--cmd-key win` para que ela seja a tecla Windows.
- **Fora de casa:** sem servidor (F3), os dois precisam estar na mesma rede, ou use o [Tailscale](https://tailscale.com) nos dois e o IP `100.x.y.z` dele.
- **Outros monitores:** `host --display 1` compartilha o segundo monitor.
- **Ajustes:** `host --bitrate-mbps 15 --fps 60`.
- **Codificador:** por padrão usa a placa de vídeo (NVIDIA, AMD ou Intel, via Media Foundation) e volta
  para o processador se ela não existir ou falhar. O título da janela mostra `(GPU)` ou `(CPU)` ao lado
  do tempo de encode. Para forçar: `host --encoder software` ou `host --encoder hardware`.

## Testar: controlar o Mac a partir do Windows

1. **No Mac**, no Terminal: `./HextRemote host`.
   Na primeira vez o macOS pede duas permissões para o **Terminal** (é ele que "abre" o Hext Remote):
   - **Gravação de Tela** — para enviar a imagem;
   - **Acessibilidade** — para receber mouse e teclado.

   Libere as duas em **Ajustes do Sistema → Privacidade e Segurança**, **feche o Terminal por completo
   (Cmd+Q)** e abra de novo. O host avisa no terminal se alguma faltar.
2. **No Windows**: `.\HextRemote.exe view <IP-do-Mac> --code ... --fingerprint ...` (o comando aparece no Mac).

- **Teclado:** no Windows, **Ctrl funciona como Command** (Ctrl+C copia no Mac) e a tecla Windows vira Control.
  Para manter o Ctrl normal: `view ... --ctrl-key ctrl`.
- **Codificador:** o Mac usa o próprio chip de vídeo (VideoToolbox) e volta para o processador se ele
  falhar; `host --encoder software` força o processador, para comparar. O título da janela no Windows
  mostra `(GPU)` ou `(CPU)` ao lado do tempo de encode.
- A imagem é capturada no tamanho "lógico" da tela (ex.: 1920×1080 num monitor 4K Retina), o que deixa
  tudo mais leve.

### Limitações atuais

- No Linux o host ainda transmite só uma imagem de teste (`--test-pattern`).
- Codificação por hardware ainda copia a imagem pela memória do sistema (Windows e Mac); o caminho direto na GPU (zero-copy) vem depois.
- Janelas abertas como administrador, a tela de UAC e a tela de bloqueio não recebem controle (precisam do serviço, na F4).
