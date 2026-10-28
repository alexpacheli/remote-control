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
| Relay próprio, sem keyframes periódicos | relay (firewall bloqueando) | 0,4 s | — | ~23 ms | ~25 ms / 35–50 ms | — |
| Direto, sem keyframes periódicos, firewall com regra para o executável | **direto** | 0,38 s | 0,44 s | ~16 ms | **~21 ms / 30–40 ms** | **0 em 22 s**; 1 no keyframe pedido após uma queda de rede de ~5 s |

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
   do que o FEC de 20% cobre. Pelo relay (TCP) não houve perda. **Confirmado:** sem keyframes periódicos
   o caminho direto ficou 22 s sem nenhuma perda; a única perda veio no keyframe pedido depois de uma queda. Isso reforça intra-refresh no lugar de
   keyframes periódicos, envio espaçado (pacing) e controle de banda adaptativo (seções 4.2 e 6.2 do
   ARCHITECTURE).

### Tarde do mesmo dia: o caminho direto parou de abrir

Com o pacing pronto (PR #7), o teste de validação ficou sempre no relay. Isso aconteceu com o executável novo e
com o antigo, com o firewall do Windows desligado nos três perfis, e com o nosso relay ou com os da n0. O
`netcheck` continuou igual ao da manhã: UDP liberado, NAT não simétrico e os mesmos IPs públicos.

Os logs do `iroh` (`RUST_LOG=iroh=debug`) mostram a mecânica funcionando. Os dois lados descobrem o próprio
endereço público pelo relay (porta UDP 7842). O visualizador recebe os candidatos do host e tenta a conexão
direta para o endereço público certo do escritório, mas nenhum pacote chega ao host. Algo entre os dois passou a
descartar UDP de entrada ao longo do dia: o firewall ou o roteador do escritório, um antivírus corporativo ou o
roteador de casa. Uma vez, pelos relays da n0, a conexão chegou a ficar direta, mas só depois de 60 s.

Pelo relay próprio, o pacing não piorou nada: 60 fps, ~24 ms e nenhuma perda. O custo foi o previsto, 2
quadros pulados a cada keyframe. A validação da perda no caminho direto fica para uma rede em que ele abra (o
spike tem `--no-pacing` para comparar).

Lições para o app:

1. **A conexão direta depende da rede e pode deixar de abrir sem aviso**, inclusive em PCs de domínio. O relay
   próprio e perto do usuário é o caminho que precisa ser sempre bom, não um plano B raro.
2. **As mensagens que combinam a conexão direta passam pelo relay.** Com o relay congestionado pelo vídeo (os
   da n0, com RTT de segundos), a conexão direta levou 60 s para abrir. O vídeo não pode lotar o caminho pelo
   relay, ou seja, o controle de banda adaptativo é necessário também para isso.
3. **Paradas de ~4 s sem nenhum pacote** aconteceram tanto no caminho direto quanto pelo relay, e o vídeo
   voltou sozinho. Parecem quedas curtas da rede do escritório. O app deve mostrar "reconectando" em vez de
   congelar sem explicação.
4. **O diagnóstico precisa vir no produto:** um modo de log do caminho (relay ou direto, e por que não ficou
   direto) e o `netcheck` dentro do app economizam horas de suporte.

### Decisão

**Adotar o `iroh` (opção B do plano).** O `hext-net` migra para ele, e os marcos 4 e 5 da F3 viram
"integrar o `iroh`" e "relay próprio" (já no ar). Antes, ou junto, entra o controle das rajadas
(intra-refresh e pacing), que vale para qualquer transporte.
