# Spike F3: vídeo sobre `iroh`

Marco 0 do [plano da F3](F3.md). A pergunta: o vídeo do Hext Remote funciona bem sobre o
[`iroh`](https://github.com/n0-computer/iroh), com o hole punching e o relay dele?

O `hext-spike` (`apps/hext-spike`) usa o mesmo empacotamento, FEC e remontagem do app, com quadros
sintéticos no formato do vídeo real (por padrão, 8 Mbps a 60 fps e um keyframe 10× maior a cada 2 s).
Ainda não usa captura nem encoder. Os relays são os públicos da n0; o relay próprio entra quando a VPS
estiver no ar.

## Como rodar

```sh
hext-spike host                 # mostra o ID
hext-spike view <ID>            # no outro computador
hext-spike view <ID> --relay-only   # o pior caso: tudo pelo relay
```

O host mostra, por segundo:

- fps e Mbps enviados;
- quadros pulados (quando a rede não dá conta);
- **quadro→ack**: o tempo desde começar a enviar um quadro até chegar a confirmação, em p50 e p95. É
  aproximadamente o RTT mais o tempo de transferir o quadro;
- o caminho em uso (direto ou relay) e o RTT.

O visualizador mostra fps, Mbps, FEC e quadros perdidos, além de quanto tempo levou para conectar e
para virar conexão direta.

## Matriz de testes

Rode cada cenário por ~1 minuto e anote as últimas linhas dos dois lados.

| # | Cenário | Caminho | Conectou em | Direto em | quadro→ack p50 / p95 | Perdas |
|---|---|---|---|---|---|---|
| 1 | Mesma rede (LAN) | | | | | |
| 2 | Casa ↔ outra casa | | | | | |
| 3 | Casa ↔ celular roteando 4G | | | | | |
| 4 | Igual ao 2, com `--relay-only` | relay | | — | | |
| 5 | Rede corporativa/restrita, se houver | | | | | |

## Critérios de decisão

- **Adotar o `iroh`:** fica direto nos cenários 1–3, ou pelo menos em 1–2; o relay entrega 8 Mbps com
  quadro→ack aceitável (< ~100 ms); e as perdas ficam baixas, com o FEC absorvendo o resto.
- **Implementação própria:** o `iroh` falha em conectar, nunca vira direto onde deveria, ou o vídeo
  sofre no relay a ponto de comprometer o uso.

## Resultados (28/10/2026)

Windows no escritório e Mac em casa, redes diferentes, sem Tailscale. 8 Mbps a 60 fps com keyframe a
cada 2 s. Relay próprio `relay.hext.com.br`, numa VM ARM da Oracle em São Paulo.

| Cenário | Caminho | Conectou em | Direto em | RTT | quadro→ack p50 / p95 | Perdas |
|---|---|---|---|---|---|---|
| Relay próprio (SP), firewall do Windows bloqueando | relay | 0,40 s | — | ~21 ms | — | 0 em 20 s |
| Relays da n0 (o mais próximo é Virgínia, EUA) | relay | 0,99 s | — | 0,4 → **6,6 s** (a fila cresce) | — | 19 em 30 s, ~3 fps |
| Relay próprio, firewall do Windows liberado | **direto** | 0,37 s | **0,46 s** | ~15 ms | **~20 ms / 40–60 ms** | ~1 quadro a cada 2 s |

O `netcheck` mostrou NAT favorável (não simétrico) e UDP liberado nos dois lados.

### O que se aprendeu

1. **O `iroh` resolve a travessia de NAT.** A conexão abre pelo relay em ~0,4 s e vira direta em ~0,5 s
   entre duas redes residenciais e corporativas comuns.
2. **O relay precisa ser próprio e perto do usuário.** Os relays públicos da n0 ficam nos EUA (nenhum na
   América do Sul) e limitam a banda: o vídeo travou em ~3,5 Mbps com o RTT subindo a segundos. O nosso
   em São Paulo entregou o vídeo inteiro com ~21 ms e nenhuma perda.
3. **O firewall do Windows bloqueia o caminho direto** de um executável não liberado; o relay continua
   funcionando. O app precisa criar a regra de firewall no instalador (F6) e lidar com o aviso no modo
   portátil.
4. **Datagramas com mais de um caminho:** o `iroh` pode informar o tamanho máximo de um caminho e enviar
   por outro menor, descartando em silêncio. O app deve limitar os datagramas a ~1100 bytes.
5. **Rajadas de keyframe causam perda no UDP direto**: ~1 quadro perdido a cada keyframe de ~170 KB, mais
   do que o FEC de 20% cobre. Pelo relay (TCP) não houve perda. Isso reforça intra-refresh no lugar de
   keyframes periódicos, envio espaçado (pacing) e controle de banda adaptativo (seções 4.2 e 6.2 do
   ARCHITECTURE).

### Decisão

**Adotar o `iroh` (opção B do plano).** O `hext-net` migra para ele, e os marcos 4 e 5 da F3 viram
"integrar o `iroh`" e "relay próprio" (já no ar). Antes, ou junto, entra o controle das rajadas
(intra-refresh e pacing), que vale para qualquer transporte.
