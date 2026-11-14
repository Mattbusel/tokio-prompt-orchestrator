{{- define "tpo.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "tpo.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else if contains (include "tpo.name" .) .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name (include "tpo.name" .) | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}

{{- define "tpo.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{ include "tpo.selectorLabels" . }}
app.kubernetes.io/version: {{ .Values.image.tag | default .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "tpo.selectorLabels" -}}
app.kubernetes.io/name: {{ include "tpo.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "tpo.secretName" -}}
{{- default (include "tpo.fullname" .) .Values.existingSecret }}
{{- end }}

{{- define "tpo.useSecret" -}}
{{- if or .Values.existingSecret (and .Values.secret.create (or .Values.secret.openaiApiKey .Values.secret.anthropicApiKey .Values.secret.apiKey)) }}true{{ end }}
{{- end }}
