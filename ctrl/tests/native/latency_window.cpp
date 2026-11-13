#include "../../../common/latency_window.h"
#include <cstdio>
int main(){
  VCLatencyWindow window;
  printf("{%s}\n",window.fields("rseg","first_fragment_to_complete_au").c_str());
  window.add(0);
  printf("{%s}\n",window.fields("rseg","first_fragment_to_complete_au").c_str());
  for(uint64_t i=1;i<=10020;i++)window.add(i*1000000ull);
  printf("{%s}\n",window.fields("pc1_seg","source_ready_to_transmit_complete").c_str());
}
