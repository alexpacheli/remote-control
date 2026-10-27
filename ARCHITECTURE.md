# Hext Remote — Plano de Arquitetura

> **Nome do produto: Hext Remote.** Executável: `HextRemote.exe` (Windows) e `Hext Remote.app` (macOS). Prefixo dos binários e crates: `hext`.
> Verificações de marca e domínio e nomes reserva estão na seção [14](#14-nome).

## 0. Visão

Software de **acesso e suporte remoto** no estilo AnyDesk / Splashtop, com a fluidez do Parsec.

**Objetivos**

- Suporte remoto **Windows e macOS** desde o início. **Linux** primeiro como cliente e depois como host.
- Conectar em alguém por um **ID numérico** e um pedido de aceite, ou **sem aceite** nas máquinas da própria conta.
- Ser **mais fluido** que as alternativas: cursor sem atraso, sem engasgo em rede ruim e consumo de banda quase zero com a tela parada.
- **Multi-monitor de primeira classe**: troca instantânea entre telas, todas ao mesmo tempo ou uma janela por monitor.
- **Criptografia de ponta a ponta**: o servidor apresenta os peers, mas nunca vê a tela.

**Fora do escopo inicial:** streaming de jogos com gamepad, clientes mobile e cliente web. Ficam para fases futuras.

---

## 1. Matriz de plataformas

| Plataforma | Cliente (quem acessa) | Host (quem é acessado) | Fase |
|---|---|---|---|
| Windows 10/11 (x64, ARM64) | ✅ | ✅ (com serviço e tela de login/UAC) | F1 |
| macOS 13+ (Intel e Apple Silicon) | ✅ | ✅ (com permissões TCC) | F2 |
| Linux (X11 e Wayland) | ✅ | X11 em F7, Wayland em seguida | F7 |

O Linux como host é viável, mas no Wayland cada sessão abre um pedido de permissão (portal), o que atrapalha o acesso não assistido. Por isso ele vem depois.

---

## 2. Visão geral

```
                 ┌──────────────────── VPS ─────────────────────┐
                 │  API / Contas   Sinalização   STUN   Relay   │
                 │  (HTTPS)        (WSS :443)    (UDP)  (UDP +  │
                 │                                       TCP443)│
                 └────────┬──────────────┬────────────────┬─────┘
                          │ apresenta    │                │ fallback
                          │ os peers     │                │ (~10–15%)
   ┌──────────────┐       ▼              ▼                ▼       ┌──────────────┐
   │ Hext Remote  │ ◄════════ P2P direto (QUIC sobre UDP) ══════► │ Hext Remote  │
   │  (cliente)   │           criptografado ponta a ponta         │  (host)      │
   └──────────────┘                                               └──────────────┘
```

- **Um único app faz os dois papéis.** Como no AnyDesk, quem instala pode acessar e ser acessado.
- O **VPS** cuida de identidade, presença, apresentação dos peers e relay. O conteúdo da sessão nunca passa decifrado por ele.

---

## 3. O aplicativo

### 3.1 Executáveis gerados

O Rust compila para **executável nativo**, sem runtime, sem .NET e sem Java:

| SO | Saída | Distribuição |
|---|---|---|
| Windows | `HextRemote.exe` (~10–20 MB) | `.exe` portátil e instalador `.msi` |
| macOS | `Hext Remote.app` (binário universal x86_64 + arm64) | `.dmg` assinado e notarizado |
| Linux | binário ELF | `.AppImage`, `.deb` e `.rpm` |

### 3.2 Modos de execução

1. **Suporte rápido (portátil):** a pessoa baixa e abre o `.exe`, sem instalar e sem precisar de admin. Ela mostra o ID e aceita a conexão. É o caso mais comum no suporte.
2. **Instalado:**
   - Roda como serviço.
   - Inicia junto com o sistema e funciona na tela de login e nos prompts de UAC.
   - Permite **acesso não assistido** com senha fixa ou pela conta.
   - Reconecta sozinho depois de um reboot (seção [3.3](#33-inicialização-automática-e-reconexão)).

### 3.3 Inicialização automática e reconexão

**Configurações do app** (aba "Segurança / Inicialização"):

| Opção | Padrão quando instalado | Efeito |
|---|---|---|
| **Iniciar com o sistema** | ligado | O serviço sobe junto com o SO, **antes do login**, e a máquina fica online com o seu ID |
| **Reconectar após reinício** | ligado | Depois de um reboot pedido na sessão, o técnico volta sozinho, sem novo pedido de aceite |
| **Acesso não assistido** | desligado | Libera a entrada com senha fixa ou pela conta, sem ninguém do outro lado |

No **modo portátil** não há inicialização automática. A tela mostra um botão "Instalar Hext Remote" que liga essas opções.

**Como cada SO inicia o Hext Remote**

| SO | Mecanismo |
|---|---|
| Windows | Serviço `hext-service` com inicialização **Automática** (não atrasada, para estar pronto já na tela de login) e reinício automático em caso de falha. Registrado também em `SafeBoot\Network`, para funcionar em **Modo Seguro com rede**. O ícone na bandeja inicia por usuário via `HKCU\...\Run`. |
| macOS | **LaunchDaemon** (`RunAtLoad` + `KeepAlive`) e **LaunchAgent** para as sessões `Aqua` e `LoginWindow`, registrados via `SMAppService` (macOS 13+). Aparecem em Ajustes → Itens de Início. |
| Linux | Unidade **systemd** de sistema e autostart XDG para a interface. |

**Fluxo de reinício com reconexão**

1. O técnico clica em **Reiniciar** (ou "Reiniciar em Modo Seguro"). É preciso ter a permissão "reiniciar" concedida na sessão.
2. O host recebe um **token de reconexão**: uso único, válido por cerca de 10 minutos, amarrado àquele técnico e àquela sessão. O token fica guardado protegido pelo SO.
3. O cliente do técnico mostra *"Aguardando a máquina voltar…"*.
4. No boot, o serviço sobe, conecta na sinalização e fica online.
5. O cliente reconecta sozinho apresentando o token, sem novo pedido de aceite, e o token é descartado.

Quedas de rede no meio da sessão também reconectam sozinhas, com backoff exponencial, sem perder o estado da sessão.

### 3.4 Processos

**Windows**

```
hext-service (SYSTEM, sessão 0)
   ├─ fala com a sinalização, mantém a presença e aceita conexões
   └─ lança ──► hext-agent (na sessão do usuário ativo, ou na tela Winlogon/UAC)
                   └─ captura, encode e injeção de input
hext (UI)    ──► interface do usuário: ID, lista de máquinas, configurações
hext --view  ──► janela(s) da sessão remota (render nativo)
```

- Um serviço na sessão 0 **não consegue capturar a tela**. Por isso ele lança um *agent* na sessão ativa usando `WTSGetActiveConsoleSessionId` e `CreateProcessAsUser`.
- Quando a tela muda para Winlogon/UAC (*secure desktop*), o agent é relançado naquele desktop.
- `Ctrl+Alt+Del` remoto é feito com `SendSAS` (sas.dll), o que exige rodar como serviço.

**macOS**

- Um **LaunchDaemon** (root) mantém presença e conexões.
- Um **LaunchAgent** na sessão do usuário faz captura e input.
- A captura da tela de login exige um agent no contexto `LoginWindow`. Fica para a fase F4.

### 3.5 Interface

- **Casca da UI** (ID, lista de máquinas, login, configurações, chat): **Tauri 2**, que é rápido de desenvolver e tem visual moderno.
- **Visualizador da sessão:** janela **nativa** (`winit` + `wgpu`) num processo separado, iniciado com `hext --view`. O vídeo nunca passa por um webview.
  - Se o visualizador travar, a UI principal continua de pé.
  - Cada monitor remoto pode virar uma janela separada.

---

## 4. Pipeline de vídeo

```
Captura (GPU) ─► [mudou?] ─► Encode HW ─► Empacota + FEC ─► Rede ─► Decode HW ─► Render
   │                 │
   │                 └─ tela parada: não codifica nada (banda ≈ 0)
   └─ textura fica na GPU (zero-copy até o encoder)
```

### 4.1 Captura

| SO | API | Observações |
|---|---|---|
| Windows | **DXGI Desktop Duplication** | Informa dirty rects e funciona no secure desktop rodando como SYSTEM. Windows.Graphics.Capture fica como alternativa. |
| macOS | **ScreenCaptureKit** | Exige a permissão "Gravação de Tela". Entrega `IOSurface`, que é zero-copy para o VideoToolbox. |
| Linux X11 | XShm / XDamage | — |
| Linux Wayland | xdg-desktop-portal ScreenCast + PipeWire | Abre um prompt de permissão |

### 4.2 Codificação

- **Codec inicial: H.264.** É universal e todo hardware decodifica. HEVC e AV1 entram depois, negociados conforme o que os dois lados suportam.
- **Encoders**
  - Windows: Media Foundation (MFT de hardware), que cobre NVIDIA, AMD e Intel. Depois, SDKs diretos (NVENC/AMF/QSV) para cortar mais latência.
  - macOS: VideoToolbox.
  - Fallback em software: **openh264**. Importante no suporte, porque muitas máquinas atendidas têm GPU fraca ou estão em VM.
- **Configuração de baixa latência**
  - Sem B-frames.
  - **Intra-refresh** no lugar de keyframes periódicos, o que evita picos de banda.
  - CBR/VBR com buffer pequeno, de cerca de 1 frame.
  - Keyframe sob demanda quando houver perda grave ou troca de monitor.
- **Específico de suporte, onde o texto precisa estar nítido**
  - **Refinamento de tela parada:** quando a tela para de mudar, o host manda um quadro de alta qualidade, porque texto borrado é a maior reclamação em ferramentas de suporte.
  - Modo 4:4:4 opcional quando o decoder do cliente suportar.
  - Com a tela parada, nada é codificado. Os dirty rects da captura disparam o encode só quando algo muda.

### 4.3 Cursor no cliente

- O host envia **forma e posição do cursor** em mensagens separadas. O cursor **não** vai no vídeo.
- O cliente desenha o cursor localmente, então o mouse responde sem nenhum atraso de rede. É o maior ganho de sensação de fluidez.

### 4.4 Decodificação e render

- Decode por hardware: D3D11VA / Media Foundation no Windows, VideoToolbox no macOS, VAAPI no Linux, e software como fallback.
- Render com `wgpu` (D3D12/D3D11, Metal ou Vulkan).
- O quadro é apresentado **assim que é decodificado**, sem fila. Modo de baixa latência com tearing é opcional.
- Escalas disponíveis: ajustar à janela, 1:1 e resolução adaptativa.

### 4.5 Multi-monitor

Cada monitor remoto é um **stream independente**:

- **Troca instantânea:** todos os encoders ficam inicializados.
  - Monitores fora de foco mandam 1–5 fps em baixa resolução, o que alimenta as miniaturas.
  - Ao trocar (`Ctrl+Alt+1..9` ou clique na barra de miniaturas), o stream é promovido a taxa cheia mais keyframe. A troca leva cerca de 1 frame, sem tela preta.
- **Layouts**
  1. Um monitor por vez, com troca instantânea.
  2. Todos lado a lado na mesma janela.
  3. **Uma janela por monitor remoto.** O técnico arrasta cada janela para um dos seus próprios monitores.
- **Banda adaptativa:** o monitor com foco recebe fps e bitrate cheios, e os outros recebem menos.

### 4.6 Metas de desempenho

| Cenário | Meta glass-to-glass | Banda |
|---|---|---|
| LAN, 1080p60 | < 30 ms | 5–15 Mbps |
| Internet, mesma região | < 50 ms + RTT | 1–5 Mbps |
| Conexão ruim (4G/ADSL) | utilizável | funciona com **1 Mbps** |
| Tela parada | — | **≈ 0** (só keepalive) |

A latência é **medida desde a F1**: um overlay de estatísticas mostra captura, encode, rede, decode e render por quadro.

---

## 5. Input e demais canais

| Canal | Detalhes |
|---|---|
| **Mouse e teclado** | Windows: `SendInput` (o agent precisa estar no mesmo desktop do alvo). macOS: `CGEvent`, que exige a permissão "Acessibilidade". O input viaja com prioridade máxima e de forma redundante, para que uma perda não trave nada. Os teclados são mapeados por scancode, para funcionar com layouts diferentes (ABNT2 ↔ US). |
| **Atalhos especiais** | `Ctrl+Alt+Del`, `Win`, `Alt+Tab` e `Cmd+Tab` vão para a máquina remota quando a janela está em foco. |
| **Clipboard** | Texto, imagens e (depois) arquivos, nos dois sentidos, com opção de desativar. |
| **Arquivos** | Arrastar e soltar, mais um gerenciador de arquivos com dois painéis. Retoma transferências interrompidas. |
| **Chat** | Texto entre técnico e cliente durante a sessão. |
| **Áudio** (F7) | Windows: WASAPI loopback. macOS: ScreenCaptureKit. Codec Opus a 10 ms. |
| **Reiniciar e reconectar** | Reinicia o PC remoto (inclusive em Modo Seguro com rede) e o cliente reconecta sozinho. |
| **Privacidade** (F7) | Tela remota em preto enquanto o técnico trabalha e bloqueio do input local. |

---

## 6. Rede

### 6.1 Transporte: QUIC (`quinn`)

- **Datagramas QUIC** (não confiáveis) para vídeo, cursor e input. Um pacote perdido não bloqueia os seguintes.
- **Streams QUIC** (confiáveis) para controle, clipboard, arquivos e chat.
- O TLS 1.3 é embutido. Os certificados são derivados das chaves dos dispositivos (seção 7).
- Serialização: `serde` + `postcard`, com versão de protocolo negociada no handshake.

### 6.2 Resiliência

- **FEC** (Reed-Solomon) nos quadros de vídeo: um pacote perdido é recuperado sem retransmissão.
- **Controle de congestionamento próprio para vídeo**, baseado em atraso no estilo GCC. Ele baixa o bitrate *antes* de a fila encher, para não acumular atraso.
- **Perda grave:** o cliente pede um keyframe ou uma atualização de intra-refresh.

### 6.3 Como os peers se conectam

1. Os dois apps mantêm uma conexão **WSS na porta 443** com a sinalização. É leve e passa por proxies corporativos.
2. O cliente pede para conectar no ID `482 913 557`. A sinalização avisa o host.
3. Os dois trocam candidatos: IPs locais, IP público descoberto via STUN e o relay.
4. **Tentativas em paralelo, a primeira que responder vence:**
   - **LAN direta:** mesma rede, com descoberta por mDNS/broadcast.
   - **P2P com UDP hole punching:** funciona em cerca de 85–90% das redes.
   - **Relay UDP:** para NAT simétrica.
   - **Relay TCP/TLS na porta 443:** para firewalls corporativos que bloqueiam UDP. É essencial em suporte a empresas.
5. O QUIC é estabelecido de ponta a ponta. O relay só repassa bytes cifrados.

---

## 7. Identidade, contas e segurança

### 7.1 Dispositivo e ID

- Na primeira execução, o app gera um **par de chaves Ed25519**. A chave privada fica protegida pelo SO: DPAPI no Windows, Keychain no macOS.
- O servidor registra a chave pública e devolve um **ID de 9 dígitos**, aleatório e nunca sequencial.
- A autenticação na sinalização é feita assinando um desafio com a chave do dispositivo.

### 7.2 Formas de acesso

| Modo | Como funciona |
|---|---|
| **Atendido** (padrão) | O técnico digita o ID. O host mostra o pedido *"Técnico X quer conectar"* com Aceitar/Recusar e caixas de permissão: ver, controlar, clipboard, arquivos e reiniciar. |
| **Senha não assistida** | Senha fixa definida no host. Ela é verificada por **PAKE (SPAKE2)**, então nem o servidor nem um relay conseguem descobri-la. |
| **Conta** | Os dispositivos ficam vinculados à conta (e-mail + 2FA/passkey). O dono acessa sem pedido de aceite. Dá para compartilhar máquinas com outros técnicos, com permissões. |

### 7.3 Garantias

- **Ponta a ponta:** o QUIC/TLS entre os peers usa certificados amarrados às chaves dos dispositivos. A sinalização transmite as impressões digitais, e o cliente fixa a chave do host (TOFU) e avisa se ela mudar.
- **Contra força bruta:** limite de tentativas por ID e por IP, e bloqueio temporário do ID.
- **Lista de permitidos:** opcionalmente, só IDs ou contas autorizados podem conectar.
- **Auditoria:** registro de quem conectou, quando, por quanto tempo e com quais permissões.
- **Indicador visível no host:** uma barra mostra que há uma sessão ativa e tem um botão para encerrá-la.

---

## 8. Servidor (VPS)

**Sim, é preciso uma VPS a partir da F3.** Nas fases F1–F2 (LAN), nenhum servidor é necessário.

### 8.1 Componentes

| Serviço | Função | Tecnologia |
|---|---|---|
| `hext-server` | API HTTPS (contas, dispositivos, IDs) e sinalização WSS | Rust (`axum` + `tokio`) |
| STUN | Descoberta do IP público | embutido no `hext-server` |
| `hext-relay` | Repasse UDP e TCP/443 de bytes cifrados, pareados por token de sessão | Rust, binário separado para escalar por região |
| Banco | Contas, dispositivos, auditoria | PostgreSQL |
| Presença | Quem está online | em memória no início, Redis ao escalar |
| TLS | Certificados | Let's Encrypt (Caddy na frente ou `rustls-acme`) |

### 8.2 Dimensionamento

- **Começo:** uma VPS com 2 vCPU e 2–4 GB de RAM roda tudo (API, sinalização, STUN, relay e Postgres).
- **Local:** em **São Paulo**, se os usuários estiverem no Brasil, porque o relay precisa estar perto. Opções:
  - Oracle Cloud (free tier em SP, com bastante tráfego incluso)
  - Vultr SP
  - Magalu Cloud
- **O custo real é o tráfego do relay.**
  - Uma sessão de suporte consome em média 0,5–3 Mbps, ou cerca de 0,2–1,3 GB por hora.
  - Só cerca de 10–15% das sessões precisam de relay; o resto é P2P.
  - Escolha um provedor com muito tráfego incluso.
- **Escala:** relays adicionais por região (SP, EUA, Europa). A sinalização indica o relay mais próximo.

### 8.3 Outros custos

| Item | Por quê |
|---|---|
| Domínio | Endereços da API e do relay |
| **Apple Developer Program** (US$ 99/ano) | Obrigatório para assinar e notarizar no macOS. Sem isso, o Gatekeeper bloqueia o app. |
| **Certificado de assinatura de código Windows** | Sem ele, o SmartScreen mostra um aviso assustador, o que é ruim para um app de suporte. Opções: certificado OV/EV ou Azure Trusted Signing (verificar disponibilidade). |

---

## 9. Estrutura do repositório

```
Cargo.toml                 # workspace
crates/
  proto/                   # mensagens, versões, serialização
  net/                     # QUIC, FEC, congestionamento, NAT traversal, relay client
  capture/                 # trait Capturer + impls windows / macos / linux
  codec/                   # trait Encoder/Decoder + impls (MF, VideoToolbox, openh264…)
  input/                   # injeção de input por SO + mapeamento de teclado
  render/                  # wgpu: vídeo, cursor local, overlay de estatísticas
  session/                 # orquestração host/cliente, canais, permissões
  crypto/                  # chaves de dispositivo, PAKE, fingerprints
  platform-windows/        # serviço, agent, secure desktop, SendSAS
  platform-macos/          # LaunchDaemon/Agent, TCC
apps/
  hext/                    # app desktop (Tauri) → HextRemote.exe / Hext Remote.app
  hext-server/             # API + sinalização + STUN
  hext-relay/              # relay UDP/TCP
docs/
.github/workflows/         # CI: build e testes em windows-latest, macos-latest, ubuntu-latest
```

- O **CI no GitHub Actions** compila e testa nos três SOs a cada push. Os executáveis de Windows e macOS saem prontos para testar nas suas máquinas.
- O código específico de cada SO fica isolado com `#[cfg(target_os = "...")]` atrás de traits comuns (`Capturer`, `Encoder`, `InputInjector`…).

---

## 10. Distribuição

- **Windows:** `.exe` portátil para suporte rápido e `.msi` para instalação com serviço. Ambos assinados.
- **macOS:** `.dmg` assinado e **notarizado**.
  - No primeiro uso como host, o app guia o usuário para liberar "Gravação de Tela" e "Acessibilidade" nos Ajustes.
  - Em empresas, as permissões podem ser liberadas antes via MDM (perfil PPPC).
- **Linux:** `.AppImage`, `.deb` e `.rpm`.
- **Atualização automática:** canal estável e beta, com pacotes assinados (por exemplo, o updater do Tauri com assinatura própria).

---

## 11. Roadmap

| Fase | Entrega | Pronto quando |
|---|---|---|
| **F0 — Fundação** ✅ | Workspace, CI nos 3 SOs, esqueleto do protocolo (handshake QUIC, pinagem por impressão digital, ping por datagrama) e estatísticas de latência. O app é uma linha de comando; a UI Tauri e o overlay visual entram com o visualizador na F1. | CI verde gerando binários de Windows e macOS |
| **F1 — MVP LAN Windows** ✅ | Captura DXGI → H.264 (OpenH264 em software, igual nos 3 SOs) → QUIC por IP direto → decode → render wgpu. Mouse, teclado (scancodes) e cursor local. Código de acesso de 9 dígitos. Vídeo num stream QUIC confiável por enquanto. Pendente: codificação por hardware (Media Foundation / VideoToolbox) e vídeo por datagramas + FEC. | Controlar um PC Windows na LAN com **< 30 ms** e fluidez visível |
| **F2 — macOS** | Host e cliente macOS (ScreenCaptureKit, VideoToolbox, CGEvent, TCC) | Windows ↔ macOS nos dois sentidos, na LAN |
| **F3 — Internet** | VPS, ID numérico, sinalização, STUN, hole punching, relay UDP + TCP/443, pedido de aceite, ponta a ponta | Conectar pela internet só com o ID, inclusive atrás de NAT/firewall |
| **F4 — Essenciais de suporte** | Serviço Windows (login/UAC/Ctrl+Alt+Del), inicialização automática nos 3 SOs, senha não assistida (PAKE), clipboard, arquivos, chat, multi-monitor completo, reiniciar e reconectar, tela de login no macOS | Um técnico consegue fazer um atendimento real de ponta a ponta |
| **F5 — Contas** | Login, 2FA, catálogo de endereços, vínculo de dispositivos, compartilhamento, auditoria | Acessar as próprias máquinas sem aceite e gerenciar técnicos |
| **F6 — Distribuição** | Instaladores, assinatura, notarização, auto-update, `.exe` portátil | Usuário final instala sem avisos de segurança |
| **F7 — Extras** | Cliente e host Linux, áudio, privacidade (tela preta), gravação de sessão, HEVC/AV1, Wake-on-LAN, mobile e web | — |

---

## 12. Bibliotecas principais (Rust)

| Área | Crate |
|---|---|
| Async e rede | `tokio`, `quinn`, `rustls` |
| Windows | `windows` (windows-rs) |
| macOS | `objc2` + `objc2-screen-capture-kit`, `objc2-video-toolbox`, `core-graphics` |
| Render e janela | `wgpu`, `winit` |
| UI | `tauri` 2 |
| Codec em software | `openh264` |
| FEC | `reed-solomon-erasure` (ou similar) |
| Cripto | `ed25519-dalek`, `spake2`, `rcgen` |
| Servidor | `axum`, `sqlx` (Postgres) |
| Serialização | `serde`, `postcard` |

Referências para estudo, **sem copiar código** por causa das licenças: RustDesk (AGPL), Sunshine/Moonlight (GPL).

---

## 13. Riscos e decisões em aberto

| Item | Risco ou decisão |
|---|---|
| Captura no secure desktop (Windows) | Complexo, mas bem documentado. Validar cedo, no começo da F4. |
| Tela de login no macOS | A Apple restringe bastante. Pode ficar limitada a "reconectar após o login". |
| Wayland como host | Os prompts do portal impedem o acesso não assistido puro. Por isso ele vem depois do X11. |
| Licença do projeto | Proprietária ou open source? Afeta quais bibliotecas e referências podem ser usadas. |
| Modelo de negócio | Grátis para uso pessoal com plano pago para técnicos e empresas? Afeta limites de relay e contas. |

---

## 14. Nome

**Nome definido: Hext Remote.**

| Uso | Forma |
|---|---|
| Produto | Hext Remote |
| Executáveis | `HextRemote.exe`, `Hext Remote.app`, `hext-remote` (Linux) |
| Prefixo técnico | `hext` (crates, serviço, binários do servidor) |

**Antes de lançar, verificar:**

- Marca no **INPI** (classes 9 e 42) e no USPTO.
- Domínio, por exemplo `hextremote.com`, `hext.app` ou `hext.com.br`.
- Nomes de usuário iguais no GitHub e nas redes sociais.

**Nomes reserva**, caso "Hext" esteja ocupado (também precisam de verificação):

| Nome | Ideia |
|---|---|
| **Reachly** | Alcançar qualquer máquina |
| **Tethr** | Um "cabo" entre técnico e cliente |
| **Overpass** | Passar por cima de firewall e NAT |
| **Pertto** | Estar perto mesmo de longe (PT) |
| **Vizo** | De "visão". Curto e funciona em qualquer idioma. |
