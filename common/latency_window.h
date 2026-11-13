#pragma once
#include <algorithm>
#include <cstdint>
#include <deque>
#include <sstream>
#include <string>
#include <vector>

// Single-owner, bounded same-machine elapsed times. No cross-machine QPC arithmetic.
struct VCLatencyWindow {
  std::deque<uint64_t> values;
  void add(uint64_t ns) { values.push_back(ns); if(values.size()>10000)values.pop_front(); }
  std::string fields(const char* prefix, const char* scope) const {
    std::ostringstream out;
    out << "\"" << prefix << "_samples\":" << values.size()
        << ",\"" << prefix << "_scope\":\"" << scope << "\"";
    if(values.empty()) {
      for(const char* name:{"avg","p50","p99","max"}) out << ",\"" << prefix << "_" << name << "_ms\":null";
    } else {
      std::vector<uint64_t> ordered(values.begin(),values.end());
      std::sort(ordered.begin(),ordered.end());
      double sum=0;for(auto ns:values)sum+=double(ns);
      out << ",\"" << prefix << "_avg_ms\":" << sum/values.size()/1e6
          << ",\"" << prefix << "_p50_ms\":" << ordered[ordered.size()/2]/1e6
          << ",\"" << prefix << "_p99_ms\":" << ordered[ordered.size()*99/100]/1e6
          << ",\"" << prefix << "_max_ms\":" << ordered.back()/1e6;
    }
    return out.str();
  }
};
