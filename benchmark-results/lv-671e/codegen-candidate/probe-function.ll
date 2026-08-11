define internal fastcc void @_RINvCs4U78gRl3LSa_28map_query_allocation_profile23map_query_codegen_probeNCNvB2_18measure_projections2_0EB2_(ptr captures(address, read_provenance) %update.0.0.val, i64 %update.1.0.val) unnamed_addr #0 personality ptr @rust_eh_personality !dbg !8 {
start:
  %_2.i = alloca [8 x i8], align 8
  call void @llvm.lifetime.start.p0(ptr nonnull %_2.i), !dbg !13
  %_12.0.i.i = shl nuw i64 %update.1.0.val, 1, !dbg !17
  %_12.1.i.i = icmp slt i64 %update.1.0.val, 0, !dbg !17
  br i1 %_12.1.i.i, label %bb6.i.i, label %bb5.i.i, !dbg !32, !prof !39

bb6.i.i:                                          ; preds = %start
  br label %bb5.i.i, !dbg !40

bb5.i.i:                                          ; preds = %bb6.i.i, %start
  %_10.sroa.0.0.i.i = phi i64 [ -1, %bb6.i.i ], [ %_12.0.i.i, %start ], !dbg !41
; call __rustc::__rust_no_alloc_shim_is_unstable_v2
  tail call void @_RNvCs9wFQrvczXsK_7___rustc35___rust_no_alloc_shim_is_unstable_v2() #56, !dbg !42, !noalias !69
  %_10.i.i.i.i.i.i.i = tail call noundef dereferenceable_or_null(40) ptr @malloc(i64 noundef 40) #56, !dbg !72, !noalias !69
  %0 = icmp eq ptr %_10.i.i.i.i.i.i.i, null, !dbg !87
  br i1 %0, label %bb2.i.i.i, label %_RNvCs4U78gRl3LSa_28map_query_allocation_profile11updated_row.exit.i, !dbg !87

bb2.i.i.i:                                        ; preds = %bb5.i.i
; call alloc::alloc::handle_alloc_error
  tail call void @_RNvNtCscdodAO9FK5_5alloc5alloc18handle_alloc_error(i64 noundef 8, i64 noundef 40) #67, !dbg !89, !noalias !69
  unreachable, !dbg !89

_RNvCs4U78gRl3LSa_28map_query_allocation_profile11updated_row.exit.i: ; preds = %bb5.i.i
  %1 = atomicrmw add ptr @_RNvCs4U78gRl3LSa_28map_query_allocation_profile11ALLOC_CALLS, i64 1 monotonic, align 8, !dbg !90, !noalias !69
  %2 = atomicrmw add ptr @_RNvCs4U78gRl3LSa_28map_query_allocation_profile11ALLOC_BYTES, i64 40 monotonic, align 8, !dbg !99, !noalias !69
  store i64 1, ptr %_10.i.i.i.i.i.i.i, align 8, !dbg !103
  %_15.sroa.4.0._10.i.i.i.i.i.sroa_idx.i.i = getelementptr inbounds nuw i8, ptr %_10.i.i.i.i.i.i.i, i64 8, !dbg !103
  store i64 1, ptr %_15.sroa.4.0._10.i.i.i.i.i.sroa_idx.i.i, align 8, !dbg !103
  %_15.sroa.5.0._10.i.i.i.i.i.sroa_idx.i.i = getelementptr inbounds nuw i8, ptr %_10.i.i.i.i.i.i.i, i64 16, !dbg !103
  store i64 0, ptr %_15.sroa.5.0._10.i.i.i.i.i.sroa_idx.i.i, align 8, !dbg !103
  %_15.sroa.6.0._10.i.i.i.i.i.sroa_idx.i.i = getelementptr inbounds nuw i8, ptr %_10.i.i.i.i.i.i.i, i64 24, !dbg !103
  store i64 %_10.sroa.0.0.i.i, ptr %_15.sroa.6.0._10.i.i.i.i.i.sroa_idx.i.i, align 8, !dbg !103
  %_15.sroa.7.0._10.i.i.i.i.i.sroa_idx.i.i = getelementptr inbounds nuw i8, ptr %_10.i.i.i.i.i.i.i, i64 32, !dbg !103
  store i64 %update.1.0.val, ptr %_15.sroa.7.0._10.i.i.i.i.i.sroa_idx.i.i, align 8, !dbg !103
  %3 = icmp ne ptr %update.0.0.val, null
  tail call void @llvm.assume(i1 %3)
; call <hyphae::cell_map::CellMap<u64, alloc::sync::Arc<map_query_allocation_profile::Row>>>::insert
  %4 = tail call fastcc noundef ptr @_RNvMs0_NtCsjvlf4jUmXVz_6hyphae8cell_mapINtB5_7CellMapyINtNtCscdodAO9FK5_5alloc4sync3ArcNtCs4U78gRl3LSa_28map_query_allocation_profile3RowEE6insertB1p_(ptr nonnull %update.0.0.val, i64 noundef 0, ptr noundef nonnull %_10.i.i.i.i.i.i.i), !dbg !105
  store ptr %4, ptr %_2.i, align 8, !dbg !105
  %5 = icmp eq ptr %4, null, !dbg !106
  br i1 %5, label %_RNCNvCs4U78gRl3LSa_28map_query_allocation_profile18measure_projections2_0B3_.exit, label %bb2.i.i, !dbg !106

bb2.i.i:                                          ; preds = %_RNvCs4U78gRl3LSa_28map_query_allocation_profile11updated_row.exit.i
  %6 = atomicrmw sub ptr %4, i64 1 release, align 8, !dbg !111, !noalias !121
  %7 = icmp eq i64 %6, 1, !dbg !128
  br i1 %7, label %bb2.i.i.i.i, label %_RNCNvCs4U78gRl3LSa_28map_query_allocation_profile18measure_projections2_0B3_.exit, !dbg !128

bb2.i.i.i.i:                                      ; preds = %bb2.i.i
  fence acquire, !dbg !129
; call <alloc::sync::Arc<map_query_allocation_profile::Row>>::drop_slow
  call fastcc void @_RNvMsn_NtCscdodAO9FK5_5alloc4syncINtB5_3ArcNtCs4U78gRl3LSa_28map_query_allocation_profile3RowE9drop_slowBH_(ptr noalias noundef nonnull readonly align 8 dereferenceable(8) %_2.i), !dbg !132
  br label %_RNCNvCs4U78gRl3LSa_28map_query_allocation_profile18measure_projections2_0B3_.exit, !dbg !132

_RNCNvCs4U78gRl3LSa_28map_query_allocation_profile18measure_projections2_0B3_.exit: ; preds = %_RNvCs4U78gRl3LSa_28map_query_allocation_profile11updated_row.exit.i, %bb2.i.i, %bb2.i.i.i.i
  call void @llvm.lifetime.end.p0(ptr nonnull %_2.i), !dbg !133
  ret void, !dbg !134
}
