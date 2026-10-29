# Hext Remote

Acesso e suporte remoto para Windows, macOS e Linux, focado em fluidez.

- Plano completo: [ARCHITECTURE.md](ARCHITECTURE.md)
- Fase atual: **F2 — Mac como host** (host Windows e macOS; visualizador em Windows, macOS e Linux)

## Estrutura

| Pasta | Conteúdo |
|---|---|
| `crates/proto` | Mensagens do protocolo: handshake, vídeo, input |
| `crates/crypto` | Identidade do computador: chave Ed25519 criada na primeira execução e guardada no disco |
| `crates/signal` | Sinalização: ID de 9 dígitos, mensagens entre o app e o servidor, cliente WebSocket |
| `crates/net` | Transporte (`iroh`/QUIC): direto quando possível, relay senão; cada computador é a sua chave |
| `crates/capture` | Captura de tela (DXGI no Windows, ScreenCaptureKit no macOS) e imagem de teste |
| `crates/codec` | H.264 (codificar e decodificar): placa de vídeo (Media Foundation no Windows, VideoToolbox no Mac) ou OpenH264 em software |
| `crates/input` | Mapa de teclas e injeção de mouse/teclado (SendInput no Windows, CGEvent no macOS) |
| `crates/render` | Renderização na GPU (wgpu), YUV → RGB no shader |
| `crates/session` | Pipelines do host e do cliente |
| `crates/metrics` | Estatísticas de latência |
| `apps/hext` | O app: comandos `host` e `view` |
| `apps/hext-server` | Servidor: IDs, presença e sinalização (veja [docs/hext-server.md](docs/hext-server.md)) |

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

   Ele mostra o **endereço**, um **código de acesso** e a **chave** deste computador, além do comando
   pronto para o outro computador. Permita o acesso quando o Firewall perguntar (porta UDP 7420).
   A chave é criada na primeira execução em `%APPDATA%\HextRemote\device.key` (no Mac,
   `~/Library/Application Support/HextRemote`) e não muda: o comando do outro computador pode ser guardado.

2. **No Mac**, cole o comando mostrado pelo Windows, trocando o nome do programa:

   ```sh
   ./HextRemote view 192.168.0.10 --code 482913557 --key <chave>
   ```

   Abre uma janela com a tela do Windows. Mouse e teclado da janela controlam o Windows.
   O título da janela mostra fps, banda, tempos de captura/encode/decode e a latência (RTT).
   O visualizador decodifica na placa de vídeo quando pode (`(GPU)` ao lado do decode);
   `view ... --decoder software` força o processador.

- **Tecla Command:** por padrão funciona como Ctrl (Cmd+C copia no Windows). Use `--cmd-key win` para que ela seja a tecla Windows.
- **Fora de casa:** conecte pelo ID (abaixo); pelo IP, os dois precisam estar na mesma rede (ou no Tailscale).
- **Outros monitores:** `host --display 1` compartilha o segundo monitor.
- **Ajustes:** `host --bitrate-mbps 15 --fps 60`.
- **Codificador:** por padrão usa a placa de vídeo (NVIDIA, AMD ou Intel, via Media Foundation) e volta
  para o processador se ela não existir ou falhar. O título da janela mostra `(GPU)` ou `(CPU)` ao lado
  do tempo de encode. Para forçar: `host --encoder software` ou `host --encoder hardware`.

## Testar: conectar pelo ID (servidor de sinalização)

Em vez de IP, código e chave, o técnico digita o **ID de 9 dígitos** do host, e quem está no
host aceita ou recusa o pedido. O app usa o nosso servidor (`wss://signal.hext.com.br`, veja
[docs/deploy.md](docs/deploy.md)) por padrão:

1. **No Windows** (host): `.\HextRemote.exe host`. Ele mostra `Seu ID: 482 913 557`.
2. **No Mac** (técnico): `./HextRemote view 482913557`.
3. **No Windows**, o host pergunta `"Mac" (ID …) quer acessar este computador. Aceitar? [s/N]`. Digite
   `s` e Enter.

Outro servidor (o `hext-server` vem junto nos artefatos, para testes): `--server ws://<IP>:8080` nos dois;
`--server off` desliga os IDs (só conexão pelo IP). O nosso relay só atende computadores registrados no
nosso servidor: com outro servidor, sem servidor ou pelo IP, a conexão é só direta, a não ser que você
informe outro relay (`--relay <url>`).

Na primeira conexão, o técnico guarda a chave do host (`known_hosts`, na mesma pasta da chave). Se outro
computador aparecer com o mesmo ID, a conexão é bloqueada com um aviso.

Depois do aceite, os dois tentam se conectar **direto**, mesmo em redes diferentes (atravessando o NAT);
quando não dá (firewall corporativo, por exemplo), a conexão passa pelo **relay** em São Paulo
(`relay.hext.com.br`). O título da janela mostra `(P2P)` ou `(relay)`. `--relay off` desliga o relay;
`--relay <url>` usa outro.

## Testar: Hext Remote instalado como serviço (Windows)

Instalado, o computador fica online com o **ID da máquina** desde o boot, sem ninguém abrir o app
(F4, marco 1; veja [docs/F4.md](docs/F4.md)).

1. **No Windows**, num terminal **como administrador**: `.\HextRemote.exe service install`. Ele copia o
   app para `C:\Program Files\Hext Remote`, inicia o serviço e mostra o ID deste computador.
2. **No Mac**: `./HextRemote view <ID>`.
3. **No Windows**, aparece uma janela perguntando se aceita. Clique em **Sim**.

`service status` mostra o estado e o ID; `service uninstall` remove o serviço (`--purge` apaga também a
identidade, e o ID deixa de valer). Os logs ficam em `C:\ProgramData\HextRemote` (`service.log` e
`agent.log`), uma pasta que só administradores abrem.

Com o serviço, o técnico vê e controla também a **tela de login, o bloqueio e os prompts do UAC**, e manda
`Ctrl+Alt+Del` com **Ctrl+Alt+End** (ou Ctrl+Alt+Delete no Mac). O acesso pelo IP com código fica
desligado no serviço: só pelo ID, com aceite.

## Testar: controlar o Mac a partir do Windows

1. **No Mac**, no Terminal: `./HextRemote host`.
   Na primeira vez o macOS pede duas permissões para o **Terminal** (é ele que "abre" o Hext Remote):
   - **Gravação de Tela** — para enviar a imagem;
   - **Acessibilidade** — para receber mouse e teclado.

   Libere as duas em **Ajustes do Sistema → Privacidade e Segurança**, **feche o Terminal por completo
   (Cmd+Q)** e abra de novo. O host avisa no terminal se alguma faltar.
2. **No Windows**: `.\HextRemote.exe view <IP-do-Mac> --code ... --key ...` (o comando aparece no Mac).

- **Teclado:** no Windows, **Ctrl funciona como Command** (Ctrl+C copia no Mac) e a tecla Windows vira Control.
  Para manter o Ctrl normal: `view ... --ctrl-key ctrl`.
- **Codificador:** o Mac usa o próprio chip de vídeo (VideoToolbox) e volta para o processador se ele
  falhar; `host --encoder software` força o processador, para comparar. O título da janela no Windows
  mostra `(GPU)` ou `(CPU)` ao lado do tempo de encode.
- A imagem é capturada no tamanho "lógico" da tela (ex.: 1920×1080 num monitor 4K Retina), o que deixa
  tudo mais leve.

### Limitações atuais

- No Linux o host ainda transmite só uma imagem de teste (`--test-pattern`).
- No Mac a imagem ainda passa pela memória do sistema (1,5 bytes por pixel, já em NV12); no Windows ela vai direto da captura para o encoder pela GPU (zero-copy), exceto em placas sem processador de vídeo D3D11 ou com o encoder em outra placa (ex.: notebooks com duas GPUs), que voltam ao caminho pela memória.
- Janelas abertas como administrador, a tela de UAC e a tela de bloqueio não recebem controle (precisam do serviço, na F4).
