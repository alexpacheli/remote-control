# Situação

[English](STATUS.md) | **Português**

Em que pé está o Hext Remote: o que está pronto, o que está em conferência e o que vem a seguir. Atualizado a cada
versão. **Última atualização: novembro de 2026, versão 0.8.2.**

## Pronto

| Fase | O que entregou | Plano |
|---|---|---|
| F0–F2 | Fundação, vídeo na rede local no Windows e no macOS, codificação e decodificação pela GPU | [plano original](planos/plano-original.md#11-roadmap) |
| F3 — Internet | ID de 9 dígitos, servidor de sinalização, QUIC direto pelo `iroh`, relay próprio, pedido de aceite | [F3](planos/F3.md) |
| F4 — Essenciais de suporte | Serviço do Windows (tela de login, UAC, `Ctrl+Alt+Del`), senha de acesso, área de transferência, arquivos, chat, vários monitores, reiniciar e reconectar, técnicos vinculados, vários computadores ao mesmo tempo | [F4](planos/F4.md) |
| F5 — Contas | Contas com 2FA, lista assinada de dispositivos, entrada sem aceite para os dispositivos do dono, compartilhamento, registro de acessos | [F5](planos/F5.md) |
| F6 — Distribuição | `.msi`, `.exe` portátil, `.dmg` notarizado, `.deb`/`.rpm`/`.AppImage`, atualização automática assinada, acesso não assistido no Mac depois do login | [F6](planos/F6.md) |
| Interface no Linux | O app como técnico no Linux: conta, Meus computadores, acessar outros computadores; `.deb` e `.rpm` com as bibliotecas de que precisam (WebKitGTK) (0.8.0) | [arquitetura](../ARCHITECTURE.pt-BR.md#7-plataformas) |
| Monitor virtual | Sessão num computador sem monitor (driver do Windows como opção no instalador, macOS sem instalar nada), resolução pelo `Ctrl+Alt+0`; uma tela que fica preta (monitor desligado no botão) passa para o virtual sozinha; o visualizador volta ao último monitor escolhido em cada computador (0.8.2) | [monitor-virtual](monitor-virtual.md) |
| Tela de privacidade (Windows) | `Ctrl+Alt+P` no visualizador: quem está no computador vê uma tela preta com um aviso e o mouse e o teclado dele não fazem nada, enquanto o técnico vê e controla tudo; `Ctrl+Alt+Del` lá desliga | [F7](planos/F7.md#marco-1-como-funciona-e-roteiro) |
| Som | O que o host toca (Windows, e macOS 13+) chega ao visualizador, uns 70 ms atrás na rede local; `Ctrl+Alt+S` silencia, e nada trafega com o computador em silêncio | [F7](planos/F7.md#marco-3-como-funciona-e-roteiro) |

Os nomes em "Meus computadores" seguem o nome atual de cada computador (0.7.1).

## Em conferência no campo

Cada fase fecha com um roteiro em máquinas reais ([roteiros/](roteiros/)):

- [ ] **F3:** casa ↔ 4G (CGNAT), casa ↔ casa e uma rede com UDP totalmente bloqueado ([F3, seção 9](planos/F3.md#9-pendências-testar-no-final)).
- [ ] **F4:** reiniciar pelo técnico e voltar sozinho, no Windows e no Mac; depois, o roteiro de atendimento
      completo de novo ([atendimento-campo](roteiros/atendimento-campo.md)).
- [ ] **F5:** o roteiro das contas ([atendimento-campo-f5](roteiros/atendimento-campo-f5.md)).
- [ ] **F6:** instalar, atualizar e desinstalar em máquinas reais ([instalacao-campo](roteiros/instalacao-campo.md)).
- [ ] **Monitor virtual:** resolução, monitor fantasma (um que perdeu o cabo) e desinstalação em máquinas reais;
      a tela de login só com o monitor virtual ([monitor-virtual](monitor-virtual.md)). Monitor desligado no botão:
      conferido (novembro de 2026).
- [ ] **Linux:** instalar o `.deb` num Ubuntu e o `.rpm` num Fedora, abrir pelo menu, entrar na conta e acessar
      um computador (vídeo, teclado, mouse, arquivo recebido).
- [x] **Janela da sessão:** cada sessão desenha numa thread própria. No Mac, uma janela minimizada volta sem
      travar; abrir outro computador não para mais as janelas abertas
      ([janela-travada](estudos/janela-travada.md#roteiro-de-campo)). Conferido no Mac (novembro de 2026).
- [ ] **Tela de privacidade:** o roteiro do marco 1 da F7 ([F7](planos/F7.md#marco-1-como-funciona-e-roteiro)).
- [ ] **Som:** o roteiro do marco 3 da F7 ([F7](planos/F7.md#marco-3-como-funciona-e-roteiro)).

## Em aberto

- Em algumas redes domésticas sem CGNAT, a conexão ainda passa pelo relay em vez de ir direto. O app já tenta
  abrir a porta no roteador (UPnP, NAT-PMP e PCP, pelo `iroh`): falta ligar o UPnP no roteador e testar.
- No Mac, depois de reiniciar, o Hext só volta quando alguém entra na conta. Fica assim de propósito
  ([decisões](DECISIONS.pt-BR.md#mac-sem-tela-de-login)): com o FileVault ligado, o macOS pede uma senha antes de carregar.
- O `.AppImage` do Linux usa o WebKitGTK do sistema (`libwebkit2gtk-4.1`): onde ele não existe, não abre.

## A seguir

- **F7 — Extras** ([plano](planos/F7.md)): tela de privacidade no Mac e host no Linux (ser acessado);
  Wake-on-LAN, gravação de sessão, HEVC/AV1, vários técnicos, mobile e web quando houver demanda.
- **Fluidez, por último e com calma:** comparar lado a lado com o Parsec, na mesma rede e sem Tailscale, e aplicar o
  que o estudo achou ([fluidez-parsec](estudos/fluidez-parsec.md)). Já entrou: a captura acompanha a cadência da tela,
  e o log mostra o maior intervalo entre quadros nos dois lados.
